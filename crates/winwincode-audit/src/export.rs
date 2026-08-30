// SPDX-License-Identifier: Apache-2.0

//! Fixed-snapshot, bounded, offline-verifiable Audit Ledger exports.
//!
//! An export contains the immutable headers needed to walk the canonical
//! organization hash chain. Retained matching events remain the already
//! secret-safe [`AuditEvent`] shape; pruned payloads become sealed
//! [`AuditDeletionProof`] values. Artifact and evidence content is never
//! copied: only digest references already committed by the event are exposed.

use std::fmt;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use winwincode_domain::{
    DeliveryId, ExecutionJobId, LeaseId, ProductSessionId, PublicationId, RepositoryId,
    Sha256Digest, StageRunId, WorkerId, WorkspaceId,
};
use winwincode_domain::{OrganizationId, ProjectId};

use crate::store::StoredHeader;
use crate::{
    AuditAccess, AuditActionKind, AuditChainCheckpoint, AuditError, AuditErrorKind, AuditEvent,
    AuditEventId, AuditOutcome, AuditRetention, AuditScope, AuditState, AuditStore,
    DataClassification, RedactionPlan, RedactionStrategy,
};

const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;
const MAX_SCAN_RECORDS: usize = 200;
const MAX_RECORD_BYTES: usize = 1_048_576;
const QUERY_DOMAIN: &[u8] = b"winwincode.audit-export-query.v1";
const CHAIN_DOMAIN: &[u8] = b"winwincode.audit-chain.v1";
const GOVERNANCE_DECISION_DOMAIN: &[u8] = b"winwincode.data-governance-decision.v1";

/// Inclusive event-time bounds for one export query.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AuditExportTimeRange {
    from_millis: u64,
    through_millis: u64,
}

impl AuditExportTimeRange {
    /// Builds an ordered, exact-range time window.
    ///
    /// # Errors
    ///
    /// Rejects zero, reversed, or non-exact JSON timestamps.
    pub fn try_new(from_millis: u64, through_millis: u64) -> Result<Self, AuditExportError> {
        if from_millis == 0 || from_millis > through_millis || through_millis > MAX_SAFE_INTEGER {
            return Err(AuditExportError::invalid(
                "audit export time range is invalid",
            ));
        }
        Ok(Self {
            from_millis,
            through_millis,
        })
    }

    #[must_use]
    pub const fn from_millis(&self) -> u64 {
        self.from_millis
    }

    #[must_use]
    pub const fn through_millis(&self) -> u64 {
        self.through_millis
    }

    const fn contains(&self, value: u64) -> bool {
        self.from_millis <= value && value <= self.through_millis
    }
}

/// Hard page bounds. Every page advances through at most `scan_records`
/// canonical headers and contains at most `max_record_bytes` encoded records.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AuditExportLimits {
    scan_records: usize,
    max_record_bytes: usize,
}

impl AuditExportLimits {
    /// Builds bounded page limits.
    ///
    /// # Errors
    ///
    /// Rejects zero or limits above the canonical hard bounds.
    pub fn try_new(scan_records: usize, max_record_bytes: usize) -> Result<Self, AuditExportError> {
        if !(1..=MAX_SCAN_RECORDS).contains(&scan_records)
            || !(1..=MAX_RECORD_BYTES).contains(&max_record_bytes)
        {
            return Err(AuditExportError::invalid(
                "audit export page bounds are outside the canonical limits",
            ));
        }
        Ok(Self {
            scan_records,
            max_record_bytes,
        })
    }

    #[must_use]
    pub const fn scan_records(&self) -> usize {
        self.scan_records
    }

    #[must_use]
    pub const fn max_record_bytes(&self) -> usize {
        self.max_record_bytes
    }
}

/// Stable subject identity used by the optional exact subject filter.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", content = "id", rename_all = "snake_case")]
enum SubjectIdentity {
    Delivery(DeliveryId),
    ProductSession(ProductSessionId),
    Lease(LeaseId),
    Publication(PublicationId),
    ExecutionJob(ExecutionJobId),
    StageRun(StageRunId),
    Worker(WorkerId),
}

/// Validated exact subject filter. Fields remain sealed so a cursor cannot
/// introduce an unvalidated product identity.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AuditSubjectFilter(SubjectIdentity);

impl AuditSubjectFilter {
    /// Builds an exact Delivery filter.
    ///
    /// # Errors
    ///
    /// Rejects a non-canonical Delivery identity.
    pub fn delivery(id: DeliveryId) -> Result<Self, AuditExportError> {
        Self::typed("dlv", SubjectIdentity::Delivery(id))
    }

    /// Builds an exact `ProductSession` filter.
    ///
    /// # Errors
    ///
    /// Rejects a non-canonical `ProductSession` identity.
    pub fn product_session(id: ProductSessionId) -> Result<Self, AuditExportError> {
        Self::typed("psn", SubjectIdentity::ProductSession(id))
    }

    /// Builds an exact Lease filter.
    ///
    /// # Errors
    ///
    /// Rejects a non-canonical Lease identity.
    pub fn lease(id: LeaseId) -> Result<Self, AuditExportError> {
        Self::typed("lse", SubjectIdentity::Lease(id))
    }

    /// Builds an exact Publication filter.
    ///
    /// # Errors
    ///
    /// Rejects a non-canonical Publication identity.
    pub fn publication(id: PublicationId) -> Result<Self, AuditExportError> {
        Self::typed("pub", SubjectIdentity::Publication(id))
    }

    /// Builds an exact `ExecutionJob` filter.
    ///
    /// # Errors
    ///
    /// Rejects a non-canonical `ExecutionJob` identity.
    pub fn execution_job(id: ExecutionJobId) -> Result<Self, AuditExportError> {
        Self::typed("job", SubjectIdentity::ExecutionJob(id))
    }

    /// Builds an exact `StageRun` filter.
    ///
    /// # Errors
    ///
    /// Rejects a non-canonical `StageRun` identity.
    pub fn stage_run(id: StageRunId) -> Result<Self, AuditExportError> {
        Self::typed("run", SubjectIdentity::StageRun(id))
    }

    /// Builds an exact Worker filter.
    ///
    /// # Errors
    ///
    /// Rejects a non-canonical Worker identity.
    pub fn worker(id: WorkerId) -> Result<Self, AuditExportError> {
        Self::typed("wrk", SubjectIdentity::Worker(id))
    }

    fn typed(prefix: &str, identity: SubjectIdentity) -> Result<Self, AuditExportError> {
        let value = match &identity {
            SubjectIdentity::Delivery(value) => &value.0,
            SubjectIdentity::ProductSession(value) => &value.0,
            SubjectIdentity::Lease(value) => &value.0,
            SubjectIdentity::Publication(value) => &value.0,
            SubjectIdentity::ExecutionJob(value) => &value.0,
            SubjectIdentity::StageRun(value) => &value.0,
            SubjectIdentity::Worker(value) => &value.0,
        };
        if !canonical_id(value, prefix) {
            return Err(AuditExportError::invalid(
                "audit export subject identity is not canonical",
            ));
        }
        Ok(Self(identity))
    }

    fn matches(&self, event: &AuditEvent) -> bool {
        let subject = event.subject();
        match &self.0 {
            SubjectIdentity::Delivery(id) => {
                subject.delivery_id().or_else(|| {
                    subject
                        .execution()
                        .map(crate::AuditExecutionIdentity::delivery_id)
                }) == Some(id)
            }
            SubjectIdentity::ProductSession(id) => {
                subject.product_session_id().or_else(|| {
                    subject
                        .execution()
                        .map(crate::AuditExecutionIdentity::product_session_id)
                }) == Some(id)
            }
            SubjectIdentity::Lease(id) => {
                subject.lease_id().or_else(|| {
                    subject
                        .execution()
                        .map(crate::AuditExecutionIdentity::lease_id)
                }) == Some(id)
            }
            SubjectIdentity::Publication(id) => subject.publication_id() == Some(id),
            SubjectIdentity::ExecutionJob(id) => {
                subject
                    .execution()
                    .map(crate::AuditExecutionIdentity::execution_job_id)
                    == Some(id)
            }
            SubjectIdentity::StageRun(id) => {
                subject
                    .execution()
                    .map(crate::AuditExecutionIdentity::stage_run_id)
                    == Some(id)
            }
            SubjectIdentity::Worker(id) => {
                subject
                    .execution()
                    .map(crate::AuditExecutionIdentity::worker_id)
                    == Some(id)
            }
        }
    }

    fn validate(&self) -> Result<(), AuditExportError> {
        let prefix = match &self.0 {
            SubjectIdentity::Delivery(_) => "dlv",
            SubjectIdentity::ProductSession(_) => "psn",
            SubjectIdentity::Lease(_) => "lse",
            SubjectIdentity::Publication(_) => "pub",
            SubjectIdentity::ExecutionJob(_) => "job",
            SubjectIdentity::StageRun(_) => "run",
            SubjectIdentity::Worker(_) => "wrk",
        };
        Self::typed(prefix, self.0.clone()).map(drop)
    }
}

/// Governance proof attached to the export. The source and policy digests
/// allow an auditor to identify the exact redaction decision without copying
/// the governed source content.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AuditExportPolicyProof {
    audit_event_id: AuditEventId,
    scope: AuditScope,
    source_digest: Sha256Digest,
    classification: DataClassification,
    resident_region: String,
    evaluated_at_millis: u64,
    decision_digest: Sha256Digest,
    strategy: RedactionStrategy,
    rule_id: String,
    rule_version: u64,
    rule_digest: Sha256Digest,
}

impl AuditExportPolicyProof {
    fn from_plan(plan: &RedactionPlan, audit_event_id: AuditEventId) -> Self {
        Self {
            audit_event_id,
            scope: plan.scope().clone(),
            source_digest: plan.source_digest().clone(),
            classification: plan.classification(),
            resident_region: plan.resident_region().as_str().to_owned(),
            evaluated_at_millis: plan.evaluated_at_millis(),
            decision_digest: plan.decision_digest().clone(),
            strategy: plan.strategy(),
            rule_id: plan.rule_id().to_owned(),
            rule_version: plan.rule_version(),
            rule_digest: plan.rule_digest().clone(),
        }
    }

    #[must_use]
    pub const fn audit_event_id(&self) -> &AuditEventId {
        &self.audit_event_id
    }

    #[must_use]
    pub const fn source_digest(&self) -> &Sha256Digest {
        &self.source_digest
    }

    #[must_use]
    pub const fn decision_digest(&self) -> &Sha256Digest {
        &self.decision_digest
    }

    #[must_use]
    pub const fn strategy(&self) -> RedactionStrategy {
        self.strategy
    }

    #[must_use]
    pub fn rule_id(&self) -> &str {
        &self.rule_id
    }

    #[must_use]
    pub const fn rule_version(&self) -> u64 {
        self.rule_version
    }

    #[must_use]
    pub const fn rule_digest(&self) -> &Sha256Digest {
        &self.rule_digest
    }
}

/// Serializable query facts committed by every cursor and export page.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AuditExportManifest {
    scope: AuditScope,
    time_range: AuditExportTimeRange,
    subject: Option<AuditSubjectFilter>,
    as_of_millis: u64,
    limits: AuditExportLimits,
    policy: AuditExportPolicyProof,
}

impl AuditExportManifest {
    #[must_use]
    pub const fn scope(&self) -> &AuditScope {
        &self.scope
    }

    #[must_use]
    pub const fn time_range(&self) -> AuditExportTimeRange {
        self.time_range
    }

    #[must_use]
    pub const fn subject(&self) -> Option<&AuditSubjectFilter> {
        self.subject.as_ref()
    }

    #[must_use]
    pub const fn as_of_millis(&self) -> u64 {
        self.as_of_millis
    }

    #[must_use]
    pub const fn limits(&self) -> AuditExportLimits {
        self.limits
    }

    #[must_use]
    pub const fn policy(&self) -> &AuditExportPolicyProof {
        &self.policy
    }

    fn header_matches(&self, header: &AuditExportHeader) -> bool {
        scope_contains(&self.scope, &header.scope)
            && self.time_range.contains(header.occurred_at_millis)
    }

    fn event_matches(&self, event: &AuditEvent) -> bool {
        self.subject
            .as_ref()
            .is_none_or(|filter| filter.matches(event))
    }

    fn digest(&self) -> Result<Sha256Digest, AuditExportError> {
        let encoded = serde_json::to_vec(self).map_err(AuditExportError::encoding)?;
        let mut hash = Sha256::new();
        framed(&mut hash, QUERY_DOMAIN);
        framed(&mut hash, &encoded);
        Ok(Sha256Digest(format!("sha256:{:x}", hash.finalize())))
    }

    fn validate(&self) -> Result<(), AuditExportError> {
        self.scope
            .validate()
            .map_err(|_| AuditExportError::invalid("audit export scope is invalid"))?;
        AuditExportTimeRange::try_new(self.time_range.from_millis, self.time_range.through_millis)?;
        AuditExportLimits::try_new(self.limits.scan_records, self.limits.max_record_bytes)?;
        if let Some(subject) = &self.subject {
            subject.validate()?;
        }
        if self.as_of_millis == 0 || self.as_of_millis > MAX_SAFE_INTEGER {
            return Err(AuditExportError::invalid(
                "audit export observation time is invalid",
            ));
        }
        if self.time_range.through_millis > self.as_of_millis {
            return Err(AuditExportError::invalid(
                "audit export time range extends beyond its observation time",
            ));
        }
        validate_digest(&self.policy.source_digest)?;
        validate_digest(&self.policy.decision_digest)?;
        validate_digest(&self.policy.rule_digest)?;
        validate_token(&self.policy.rule_id)?;
        if !canonical_id(self.policy.audit_event_id.as_str(), "aud")
            || self.policy.scope != self.scope
            || self.policy.evaluated_at_millis == 0
            || self.policy.evaluated_at_millis > MAX_SAFE_INTEGER
            || !valid_region(&self.policy.resident_region)
            || governance_decision_digest(&self.policy) != self.policy.decision_digest
        {
            return Err(AuditExportError::corrupt(
                "audit export governance proof does not match its redaction decision",
            ));
        }
        if self.policy.rule_version == 0 || self.policy.rule_version > MAX_SAFE_INTEGER {
            return Err(AuditExportError::invalid(
                "audit export policy version is invalid",
            ));
        }
        Ok(())
    }
}

/// Authorized export query. The access value is retained separately because
/// a serializable manifest is evidence, not authorization.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuditExportQuery {
    access: AuditAccess,
    manifest: AuditExportManifest,
    query_digest: Sha256Digest,
}

impl AuditExportQuery {
    /// Builds a fixed-shape export query bound to one governance redaction
    /// decision.
    ///
    /// # Errors
    ///
    /// Rejects invalid time/page bounds or policy proof encoding.
    pub fn try_new(
        access: AuditAccess,
        time_range: AuditExportTimeRange,
        as_of_millis: u64,
        limits: AuditExportLimits,
        policy: &RedactionPlan,
        policy_audit_event_id: AuditEventId,
    ) -> Result<Self, AuditExportError> {
        let manifest = AuditExportManifest {
            scope: access.scope().clone(),
            time_range,
            subject: None,
            as_of_millis,
            limits,
            policy: AuditExportPolicyProof::from_plan(policy, policy_audit_event_id),
        };
        manifest.validate()?;
        let query_digest = manifest.digest()?;
        Ok(Self {
            access,
            manifest,
            query_digest,
        })
    }

    /// Adds one exact subject filter and rebinds the query digest.
    ///
    /// # Errors
    ///
    /// Returns an encoding error if the canonical manifest cannot be encoded.
    pub fn with_subject(mut self, subject: AuditSubjectFilter) -> Result<Self, AuditExportError> {
        self.manifest.subject = Some(subject);
        self.query_digest = self.manifest.digest()?;
        Ok(self)
    }

    #[must_use]
    pub const fn manifest(&self) -> &AuditExportManifest {
        &self.manifest
    }

    #[must_use]
    pub const fn query_digest(&self) -> &Sha256Digest {
        &self.query_digest
    }
}

/// Cursor sealed to an exact query and immutable organization checkpoint.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AuditExportCursor {
    query_digest: Sha256Digest,
    checkpoint: AuditChainCheckpoint,
    after_sequence: u64,
    previous_digest: Option<Sha256Digest>,
}

impl AuditExportCursor {
    #[must_use]
    pub const fn checkpoint(&self) -> &AuditChainCheckpoint {
        &self.checkpoint
    }

    #[must_use]
    pub const fn after_sequence(&self) -> u64 {
        self.after_sequence
    }
}

/// Immutable header fields required to recompute one organization chain link.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AuditExportHeader {
    sequence: u64,
    previous_digest: Option<Sha256Digest>,
    event_digest: Sha256Digest,
    payload_digest: Sha256Digest,
    event_id: AuditEventId,
    occurred_at_millis: u64,
    scope: AuditScope,
    retention: AuditRetention,
}

impl AuditExportHeader {
    #[must_use]
    pub const fn sequence(&self) -> u64 {
        self.sequence
    }

    #[must_use]
    pub const fn event_digest(&self) -> &Sha256Digest {
        &self.event_digest
    }

    #[must_use]
    pub const fn payload_digest(&self) -> &Sha256Digest {
        &self.payload_digest
    }

    #[must_use]
    pub const fn event_id(&self) -> &AuditEventId {
        &self.event_id
    }

    #[must_use]
    pub const fn occurred_at_millis(&self) -> u64 {
        self.occurred_at_millis
    }

    #[must_use]
    pub const fn scope(&self) -> &AuditScope {
        &self.scope
    }
}

/// Digest-only references already committed by a canonical audit event.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditArtifactDigestKind {
    StateBefore,
    StateAfter,
    StateCurrent,
    ModelInput,
    ModelOutput,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AuditArtifactDigestReference {
    kind: AuditArtifactDigestKind,
    digest: Sha256Digest,
}

impl AuditArtifactDigestReference {
    #[must_use]
    pub const fn kind(&self) -> AuditArtifactDigestKind {
        self.kind
    }

    #[must_use]
    pub const fn digest(&self) -> &Sha256Digest {
        &self.digest
    }
}

/// Immutable proof that a finite-retention payload was deleted only after its
/// deadline while its chain header remained.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AuditDeletionProof {
    pruned_at_millis: u64,
    tombstone_event_digest: Sha256Digest,
}

impl AuditDeletionProof {
    #[must_use]
    pub const fn pruned_at_millis(&self) -> u64 {
        self.pruned_at_millis
    }

    #[must_use]
    pub const fn tombstone_event_digest(&self) -> &Sha256Digest {
        &self.tombstone_event_digest
    }
}

/// Payload state for one chain witness.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AuditExportContent {
    /// Header-only witness outside the requested scope/time window.
    Witness,
    /// Retained canonical event. This type cannot represent raw commands,
    /// prompts, responses, credentials, or Artifact bodies.
    Event { event: Box<AuditEvent> },
    /// Immutable retention tombstone for a deleted event payload.
    DeletionProof { proof: AuditDeletionProof },
}

/// One bounded export record plus its filter result.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AuditExportRecord {
    header: AuditExportHeader,
    content: AuditExportContent,
    included: bool,
    artifact_references: Vec<AuditArtifactDigestReference>,
}

impl AuditExportRecord {
    #[must_use]
    pub const fn header(&self) -> &AuditExportHeader {
        &self.header
    }

    #[must_use]
    pub const fn content(&self) -> &AuditExportContent {
        &self.content
    }

    #[must_use]
    pub const fn included(&self) -> bool {
        self.included
    }

    #[must_use]
    pub fn artifact_references(&self) -> &[AuditArtifactDigestReference] {
        &self.artifact_references
    }
}

/// One fixed-snapshot page. Every record is a contiguous chain witness;
/// `included_records` selects the requested scope/time/subject results.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AuditExportPage {
    manifest: AuditExportManifest,
    query_digest: Sha256Digest,
    checkpoint: AuditChainCheckpoint,
    start_after_sequence: u64,
    start_previous_digest: Option<Sha256Digest>,
    records: Vec<AuditExportRecord>,
    record_bytes: usize,
    next_cursor: Option<AuditExportCursor>,
}

impl AuditExportPage {
    #[must_use]
    pub const fn manifest(&self) -> &AuditExportManifest {
        &self.manifest
    }

    #[must_use]
    pub const fn checkpoint(&self) -> &AuditChainCheckpoint {
        &self.checkpoint
    }

    #[must_use]
    pub fn records(&self) -> &[AuditExportRecord] {
        &self.records
    }

    pub fn included_records(&self) -> impl Iterator<Item = &AuditExportRecord> {
        self.records.iter().filter(|record| record.included)
    }

    #[must_use]
    pub const fn record_bytes(&self) -> usize {
        self.record_bytes
    }

    #[must_use]
    pub const fn next_cursor(&self) -> Option<&AuditExportCursor> {
        self.next_cursor.as_ref()
    }
}

/// State carried by an offline verifier across consecutive pages.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuditExportVerificationState {
    query_digest: Sha256Digest,
    checkpoint: AuditChainCheckpoint,
    after_sequence: u64,
    last_digest: Option<Sha256Digest>,
    policy_event_verified: bool,
    complete: bool,
}

impl AuditExportVerificationState {
    #[must_use]
    pub const fn complete(&self) -> bool {
        self.complete
    }

    #[must_use]
    pub const fn after_sequence(&self) -> u64 {
        self.after_sequence
    }
}

impl AuditStore {
    /// Reads one bounded page from a fixed, fully verified organization
    /// snapshot. Appends after the first page do not enter later cursor pages.
    ///
    /// # Errors
    ///
    /// Rejects a cursor from another query/snapshot, oversized individual
    /// records, unprovable subject matches after payload deletion, or any
    /// canonical Audit Ledger corruption.
    pub fn export_page(
        &self,
        query: &AuditExportQuery,
        cursor: Option<&AuditExportCursor>,
    ) -> Result<AuditExportPage, AuditExportError> {
        query.manifest.validate()?;
        if query.manifest.digest()? != query.query_digest {
            return Err(AuditExportError::corrupt(
                "audit export query digest does not match its manifest",
            ));
        }
        let (expected_checkpoint, after_sequence, start_previous_digest) = match cursor {
            Some(cursor) => {
                validate_cursor(query, cursor)?;
                (
                    Some(&cursor.checkpoint),
                    cursor.after_sequence,
                    cursor.previous_digest.clone(),
                )
            }
            None => (None, 0, None),
        };
        let (checkpoint, headers) = self
            .export_snapshot_headers(
                query.manifest.scope.organization_id(),
                expected_checkpoint,
                after_sequence,
                query.manifest.limits.scan_records,
            )
            .map_err(AuditExportError::store)?;
        let policy_record = self
            .read_exact(
                &query.access,
                &query.manifest.policy.audit_event_id,
                query.manifest.as_of_millis,
            )
            .map_err(AuditExportError::store)?
            .ok_or_else(|| {
                AuditExportError::snapshot(
                    "audit export governance decision is absent from the authorized scope",
                )
            })?;
        if policy_record.sequence() > checkpoint.last_sequence() {
            return Err(AuditExportError::snapshot(
                "audit export governance decision is newer than the fixed snapshot",
            ));
        }
        validate_policy_event(
            &query.manifest.policy,
            policy_record.event().ok_or_else(|| {
                AuditExportError::snapshot("audit export governance decision payload was deleted")
            })?,
        )?;
        let mut records = Vec::with_capacity(headers.len());
        let mut record_bytes = 0_usize;
        for stored in headers {
            let record = export_record(&query.manifest, stored)?;
            let encoded_len = serde_json::to_vec(&record)
                .map_err(AuditExportError::encoding)?
                .len();
            if encoded_len > query.manifest.limits.max_record_bytes {
                return Err(AuditExportError::invalid(
                    "one audit export record exceeds the configured byte bound",
                ));
            }
            if !records.is_empty()
                && record_bytes.saturating_add(encoded_len) > query.manifest.limits.max_record_bytes
            {
                break;
            }
            record_bytes = record_bytes
                .checked_add(encoded_len)
                .ok_or_else(|| AuditExportError::corrupt("audit export byte count overflow"))?;
            records.push(record);
        }
        let (last_sequence, last_digest) =
            records
                .last()
                .map_or((after_sequence, start_previous_digest.clone()), |record| {
                    (
                        record.header.sequence,
                        Some(record.header.event_digest.clone()),
                    )
                });
        let next_cursor = (last_sequence < checkpoint.last_sequence()).then(|| AuditExportCursor {
            query_digest: query.query_digest.clone(),
            checkpoint: checkpoint.clone(),
            after_sequence: last_sequence,
            previous_digest: last_digest,
        });
        Ok(AuditExportPage {
            manifest: query.manifest.clone(),
            query_digest: query.query_digest.clone(),
            checkpoint,
            start_after_sequence: after_sequence,
            start_previous_digest,
            records,
            record_bytes,
            next_cursor,
        })
    }
}

/// Stateless offline verifier for serialized export pages.
pub struct AuditExportVerifier;

impl AuditExportVerifier {
    /// Verifies one page and advances the caller-owned chain state.
    ///
    /// The first page requires `previous = None`; later pages require the exact
    /// state returned by the preceding call.
    ///
    /// # Errors
    ///
    /// Rejects changed manifests, filters, headers, payloads, tombstones,
    /// digest references, page bounds, gaps, or snapshot tails.
    pub fn verify_page(
        page: &AuditExportPage,
        previous: Option<&AuditExportVerificationState>,
    ) -> Result<AuditExportVerificationState, AuditExportError> {
        page.manifest.validate()?;
        let query_digest = page.manifest.digest()?;
        if query_digest != page.query_digest {
            return Err(AuditExportError::corrupt(
                "audit export page query digest changed",
            ));
        }
        let (mut sequence, mut previous_digest, mut policy_event_verified) =
            verification_start(page, &query_digest, previous)?;
        if page.checkpoint.organization_id() != page.manifest.scope.organization_id()
            || page.records.len() > page.manifest.limits.scan_records
        {
            return Err(AuditExportError::corrupt(
                "audit export page exceeds its manifest or organization",
            ));
        }
        let mut record_bytes = 0_usize;
        for record in &page.records {
            sequence = sequence
                .checked_add(1)
                .ok_or_else(|| AuditExportError::corrupt("audit export sequence overflow"))?;
            if record.header.sequence != sequence
                || record.header.previous_digest != previous_digest
                || record.header.scope.organization_id() != page.manifest.scope.organization_id()
            {
                return Err(AuditExportError::corrupt(
                    "audit export chain witness is missing or reordered",
                ));
            }
            verify_record(&page.manifest, record)?;
            if record.header.event_id == page.manifest.policy.audit_event_id {
                let AuditExportContent::Event { event } = &record.content else {
                    return Err(AuditExportError::corrupt(
                        "audit export governance decision is not retained",
                    ));
                };
                validate_policy_event(&page.manifest.policy, event)?;
                policy_event_verified = true;
            }
            let expected_digest = export_chain_digest(&record.header)?;
            if expected_digest != record.header.event_digest {
                return Err(AuditExportError::corrupt(
                    "audit export header digest changed",
                ));
            }
            let encoded_len = serde_json::to_vec(record)
                .map_err(AuditExportError::encoding)?
                .len();
            record_bytes = record_bytes
                .checked_add(encoded_len)
                .ok_or_else(|| AuditExportError::corrupt("audit export byte count overflow"))?;
            previous_digest = Some(record.header.event_digest.clone());
        }
        if record_bytes != page.record_bytes || record_bytes > page.manifest.limits.max_record_bytes
        {
            return Err(AuditExportError::corrupt(
                "audit export encoded byte bound changed",
            ));
        }
        let complete = page.next_cursor.is_none();
        if let Some(cursor) = &page.next_cursor {
            if cursor.query_digest != query_digest
                || cursor.checkpoint != page.checkpoint
                || cursor.after_sequence != sequence
                || cursor.previous_digest != previous_digest
            {
                return Err(AuditExportError::corrupt(
                    "audit export continuation cursor changed",
                ));
            }
        } else {
            if sequence != page.checkpoint.last_sequence()
                || previous_digest.as_ref() != page.checkpoint.last_digest()
            {
                return Err(AuditExportError::corrupt(
                    "audit export final page does not reach its checkpoint",
                ));
            }
            if !policy_event_verified {
                return Err(AuditExportError::corrupt(
                    "audit export does not contain its governance decision",
                ));
            }
        }
        Ok(AuditExportVerificationState {
            query_digest,
            checkpoint: page.checkpoint.clone(),
            after_sequence: sequence,
            last_digest: previous_digest,
            policy_event_verified,
            complete,
        })
    }

    /// Decodes and verifies a JSON page.
    ///
    /// # Errors
    ///
    /// Rejects non-canonical JSON or any verification failure.
    pub fn verify_json(
        encoded: &[u8],
        previous: Option<&AuditExportVerificationState>,
    ) -> Result<(AuditExportPage, AuditExportVerificationState), AuditExportError> {
        let page: AuditExportPage = serde_json::from_slice(encoded)
            .map_err(|_| AuditExportError::corrupt("audit export JSON is invalid"))?;
        let state = Self::verify_page(&page, previous)?;
        Ok((page, state))
    }
}

fn verification_start(
    page: &AuditExportPage,
    query_digest: &Sha256Digest,
    previous: Option<&AuditExportVerificationState>,
) -> Result<(u64, Option<Sha256Digest>, bool), AuditExportError> {
    if let Some(previous) = previous {
        if previous.complete
            || &previous.query_digest != query_digest
            || previous.checkpoint != page.checkpoint
            || previous.after_sequence != page.start_after_sequence
            || previous.last_digest != page.start_previous_digest
        {
            return Err(AuditExportError::snapshot(
                "audit export page does not continue the verified snapshot",
            ));
        }
        return Ok((
            previous.after_sequence,
            previous.last_digest.clone(),
            previous.policy_event_verified,
        ));
    }
    if page.start_after_sequence != 0 || page.start_previous_digest.is_some() {
        return Err(AuditExportError::snapshot(
            "audit export verification must start at sequence zero",
        ));
    }
    Ok((0, None, false))
}

fn validate_cursor(
    query: &AuditExportQuery,
    cursor: &AuditExportCursor,
) -> Result<(), AuditExportError> {
    validate_digest(&cursor.query_digest)?;
    if cursor.query_digest != query.query_digest
        || cursor.checkpoint.organization_id() != query.manifest.scope.organization_id()
        || cursor.after_sequence > cursor.checkpoint.last_sequence()
        || (cursor.after_sequence == 0) != cursor.previous_digest.is_none()
        || cursor.after_sequence == cursor.checkpoint.last_sequence()
    {
        return Err(AuditExportError::snapshot(
            "audit export cursor does not match the requested fixed snapshot",
        ));
    }
    if let Some(digest) = &cursor.previous_digest {
        validate_digest(digest)?;
    }
    Ok(())
}

fn export_record(
    manifest: &AuditExportManifest,
    stored: StoredHeader,
) -> Result<AuditExportRecord, AuditExportError> {
    let header = header_from_stored(&stored)?;
    let header_matches = manifest.header_matches(&header);
    let is_policy_event = header.event_id == manifest.policy.audit_event_id;
    let (content, included, artifact_references) = match stored.payload {
        Some(payload) if header_matches || is_policy_event => {
            if sha256_digest(&payload) != header.payload_digest {
                return Err(AuditExportError::corrupt(
                    "audit export payload digest changed",
                ));
            }
            let event: AuditEvent = serde_json::from_slice(&payload)
                .map_err(|_| AuditExportError::corrupt("audit export payload is invalid"))?;
            verify_event_header(&header, &event)?;
            let included = header_matches && manifest.event_matches(&event);
            let references = artifact_references(&event);
            (
                AuditExportContent::Event {
                    event: Box::new(event),
                },
                included,
                references,
            )
        }
        None if is_policy_event => {
            return Err(AuditExportError::snapshot(
                "audit export governance decision payload was deleted",
            ));
        }
        None if header_matches && manifest.subject.is_some() => {
            return Err(AuditExportError::snapshot(
                "a pruned audit payload cannot prove the requested subject filter",
            ));
        }
        None if header_matches => {
            let pruned_at_millis = stored
                .payload_pruned_at_millis
                .and_then(|value| u64::try_from(value).ok())
                .ok_or_else(|| {
                    AuditExportError::corrupt("audit deletion proof timestamp is invalid")
                })?;
            let tombstone_event_digest = stored.tombstone_event_digest.ok_or_else(|| {
                AuditExportError::corrupt("audit deletion proof digest is missing")
            })?;
            if pruned_at_millis > manifest.as_of_millis {
                return Err(AuditExportError::snapshot(
                    "audit deletion proof is newer than the fixed observation time",
                ));
            }
            (
                AuditExportContent::DeletionProof {
                    proof: AuditDeletionProof {
                        pruned_at_millis,
                        tombstone_event_digest,
                    },
                },
                true,
                Vec::new(),
            )
        }
        Some(_) | None => (AuditExportContent::Witness, false, Vec::new()),
    };
    Ok(AuditExportRecord {
        header,
        content,
        included,
        artifact_references,
    })
}

fn header_from_stored(stored: &StoredHeader) -> Result<AuditExportHeader, AuditExportError> {
    let occurred_at_millis = u64::try_from(stored.occurred_at_millis)
        .map_err(|_| AuditExportError::corrupt("audit export timestamp is negative"))?;
    let scope = stored_scope(stored)?;
    let retention = match (
        stored.retention_kind.as_str(),
        stored.retention_until_millis,
    ) {
        ("indefinite", None) => AuditRetention::Indefinite,
        ("until", Some(value)) => AuditRetention::UntilMillis(
            u64::try_from(value)
                .map_err(|_| AuditExportError::corrupt("audit retention time is negative"))?,
        ),
        _ => {
            return Err(AuditExportError::corrupt(
                "audit export retention header is invalid",
            ));
        }
    };
    Ok(AuditExportHeader {
        sequence: stored.sequence,
        previous_digest: stored.previous_digest.clone(),
        event_digest: stored.event_digest.clone(),
        payload_digest: stored.payload_digest.clone(),
        event_id: AuditEventId::try_new(stored.event_id.clone())
            .map_err(AuditExportError::store)?,
        occurred_at_millis,
        scope,
        retention,
    })
}

fn stored_scope(stored: &StoredHeader) -> Result<AuditScope, AuditExportError> {
    let organization = OrganizationId(stored.organization_id.clone());
    match (
        stored.workspace_id.as_ref(),
        stored.project_id.as_ref(),
        stored.repository_id.as_ref(),
    ) {
        (None, None, None) => AuditScope::organization(organization),
        (Some(workspace), None, None) => {
            AuditScope::workspace(organization, WorkspaceId(workspace.clone()))
        }
        (Some(workspace), Some(project), None) => AuditScope::project(
            organization,
            WorkspaceId(workspace.clone()),
            ProjectId(project.clone()),
        ),
        (Some(workspace), Some(project), Some(repository)) => AuditScope::repository(
            organization,
            WorkspaceId(workspace.clone()),
            ProjectId(project.clone()),
            RepositoryId(repository.clone()),
        ),
        _ => Err(AuditError::invalid(
            "audit export scope hierarchy is invalid",
        )),
    }
    .map_err(AuditExportError::store)
}

fn verify_record(
    manifest: &AuditExportManifest,
    record: &AuditExportRecord,
) -> Result<(), AuditExportError> {
    validate_digest(&record.header.event_digest)?;
    validate_digest(&record.header.payload_digest)?;
    if let Some(digest) = &record.header.previous_digest {
        validate_digest(digest)?;
    }
    let header_matches = manifest.header_matches(&record.header);
    match &record.content {
        AuditExportContent::Witness => {
            if header_matches || record.included || !record.artifact_references.is_empty() {
                return Err(AuditExportError::corrupt(
                    "audit export witness does not match its filter",
                ));
            }
        }
        AuditExportContent::Event { event } => {
            let is_policy_event = record.header.event_id == manifest.policy.audit_event_id;
            if !(header_matches || is_policy_event)
                || sha256_digest(&serde_json::to_vec(event).map_err(AuditExportError::encoding)?)
                    != record.header.payload_digest
            {
                return Err(AuditExportError::corrupt(
                    "audit export event payload does not match its header",
                ));
            }
            verify_event_header(&record.header, event)?;
            if record.included != (header_matches && manifest.event_matches(event))
                || record.artifact_references != artifact_references(event)
            {
                return Err(AuditExportError::corrupt(
                    "audit export event filter or digest references changed",
                ));
            }
        }
        AuditExportContent::DeletionProof { proof } => {
            let AuditRetention::UntilMillis(retention_until) = record.header.retention else {
                return Err(AuditExportError::corrupt(
                    "indefinite audit payload has a deletion proof",
                ));
            };
            if !header_matches
                || manifest.subject.is_some()
                || !record.included
                || !record.artifact_references.is_empty()
                || proof.pruned_at_millis < retention_until
                || proof.pruned_at_millis > manifest.as_of_millis
                || proof.tombstone_event_digest != record.header.event_digest
            {
                return Err(AuditExportError::corrupt(
                    "audit export deletion proof is invalid",
                ));
            }
        }
    }
    Ok(())
}

fn verify_event_header(
    header: &AuditExportHeader,
    event: &AuditEvent,
) -> Result<(), AuditExportError> {
    if event.event_id() != &header.event_id
        || event.occurred_at_millis() != header.occurred_at_millis
        || event.scope() != &header.scope
        || event.retention() != header.retention
    {
        return Err(AuditExportError::corrupt(
            "audit export event does not match its immutable header",
        ));
    }
    Ok(())
}

fn validate_policy_event(
    proof: &AuditExportPolicyProof,
    event: &AuditEvent,
) -> Result<(), AuditExportError> {
    let expected_action = format!("{}.v{}", proof.rule_id, proof.rule_version);
    let decision_matches = matches!(
        event.state(),
        AuditState::Unchanged {
            current: Some(digest)
        } if digest == &proof.decision_digest
    );
    if event.event_id() != &proof.audit_event_id
        || event.scope() != &proof.scope
        || event.action().kind() != AuditActionKind::Policy
        || event.action().name() != expected_action
        || event.outcome() != AuditOutcome::Succeeded
        || event.result_code() != "redaction-planned"
        || event.retention() != AuditRetention::Indefinite
        || !decision_matches
    {
        return Err(AuditExportError::corrupt(
            "audit export governance decision event is invalid",
        ));
    }
    Ok(())
}

fn artifact_references(event: &AuditEvent) -> Vec<AuditArtifactDigestReference> {
    let mut references = Vec::new();
    match event.state() {
        AuditState::Changed { before, after } => {
            if let Some(before) = before {
                references.push(AuditArtifactDigestReference {
                    kind: AuditArtifactDigestKind::StateBefore,
                    digest: before.clone(),
                });
            }
            references.push(AuditArtifactDigestReference {
                kind: AuditArtifactDigestKind::StateAfter,
                digest: after.clone(),
            });
        }
        AuditState::Unchanged { current } => {
            if let Some(current) = current {
                references.push(AuditArtifactDigestReference {
                    kind: AuditArtifactDigestKind::StateCurrent,
                    digest: current.clone(),
                });
            }
        }
    }
    if let Some(summary) = event.action().model_summary() {
        references.push(AuditArtifactDigestReference {
            kind: AuditArtifactDigestKind::ModelInput,
            digest: summary.input_digest().clone(),
        });
        references.push(AuditArtifactDigestReference {
            kind: AuditArtifactDigestKind::ModelOutput,
            digest: summary.output_digest().clone(),
        });
    }
    references
}

fn governance_decision_digest(proof: &AuditExportPolicyProof) -> Sha256Digest {
    let mut hash = Sha256::new();
    framed(&mut hash, GOVERNANCE_DECISION_DOMAIN);
    framed(&mut hash, proof.rule_id.as_bytes());
    hash.update(proof.rule_version.to_be_bytes());
    framed(&mut hash, proof.rule_digest.0.as_bytes());
    framed(&mut hash, proof.scope.organization_id().0.as_bytes());
    frame_optional(
        &mut hash,
        proof.scope.workspace_id().map(|value| value.0.as_bytes()),
    );
    frame_optional(
        &mut hash,
        proof.scope.project_id().map(|value| value.0.as_bytes()),
    );
    frame_optional(
        &mut hash,
        proof.scope.repository_id().map(|value| value.0.as_bytes()),
    );
    framed(&mut hash, proof.source_digest.0.as_bytes());
    framed(&mut hash, classification_token(proof.classification));
    framed(&mut hash, proof.resident_region.as_bytes());
    framed(&mut hash, b"redaction");
    framed(&mut hash, redaction_token(proof.strategy));
    hash.update(proof.evaluated_at_millis.to_be_bytes());
    Sha256Digest(format!("sha256:{:x}", hash.finalize()))
}

const fn classification_token(classification: DataClassification) -> &'static [u8] {
    match classification {
        DataClassification::Public => b"public",
        DataClassification::Internal => b"internal",
        DataClassification::Confidential => b"confidential",
        DataClassification::Restricted => b"restricted",
        DataClassification::Secret => b"secret",
    }
}

const fn redaction_token(strategy: RedactionStrategy) -> &'static [u8] {
    match strategy {
        RedactionStrategy::Reveal => b"reveal",
        RedactionStrategy::Mask => b"mask",
        RedactionStrategy::Hash => b"hash",
        RedactionStrategy::Remove => b"remove",
    }
}

fn valid_region(value: &str) -> bool {
    (2..=64).contains(&value.len())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        && value
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphanumeric)
        && value
            .as_bytes()
            .last()
            .is_some_and(u8::is_ascii_alphanumeric)
}

fn export_chain_digest(header: &AuditExportHeader) -> Result<Sha256Digest, AuditExportError> {
    let mut hash = Sha256::new();
    framed(&mut hash, CHAIN_DOMAIN);
    framed(&mut hash, header.scope.organization_id().0.as_bytes());
    hash.update(header.sequence.to_be_bytes());
    framed(
        &mut hash,
        header
            .previous_digest
            .as_ref()
            .map_or(&[][..], |digest| digest.0.as_bytes()),
    );
    framed(&mut hash, header.event_id.as_str().as_bytes());
    hash.update(
        i64::try_from(header.occurred_at_millis)
            .map_err(|_| AuditExportError::corrupt("audit export time exceeds SQLite range"))?
            .to_be_bytes(),
    );
    framed(
        &mut hash,
        header
            .scope
            .workspace_id()
            .map_or(&[][..], |value| value.0.as_bytes()),
    );
    framed(
        &mut hash,
        header
            .scope
            .project_id()
            .map_or(&[][..], |value| value.0.as_bytes()),
    );
    framed(
        &mut hash,
        header
            .scope
            .repository_id()
            .map_or(&[][..], |value| value.0.as_bytes()),
    );
    match header.retention {
        AuditRetention::Indefinite => {
            framed(&mut hash, b"indefinite");
            hash.update(0_i64.to_be_bytes());
        }
        AuditRetention::UntilMillis(value) => {
            framed(&mut hash, b"until");
            hash.update(
                i64::try_from(value)
                    .map_err(|_| {
                        AuditExportError::corrupt("audit export retention exceeds SQLite range")
                    })?
                    .to_be_bytes(),
            );
        }
    }
    framed(&mut hash, header.payload_digest.0.as_bytes());
    Ok(Sha256Digest(format!("sha256:{:x}", hash.finalize())))
}

fn scope_contains(container: &AuditScope, candidate: &AuditScope) -> bool {
    container.organization_id() == candidate.organization_id()
        && container
            .workspace_id()
            .is_none_or(|id| candidate.workspace_id() == Some(id))
        && container
            .project_id()
            .is_none_or(|id| candidate.project_id() == Some(id))
        && container
            .repository_id()
            .is_none_or(|id| candidate.repository_id() == Some(id))
}

fn validate_digest(digest: &Sha256Digest) -> Result<(), AuditExportError> {
    let valid = digest.0.strip_prefix("sha256:").is_some_and(|value| {
        value.len() == 64
            && value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    });
    if valid {
        Ok(())
    } else {
        Err(AuditExportError::corrupt(
            "audit export contains a malformed digest",
        ))
    }
}

fn validate_token(value: &str) -> Result<(), AuditExportError> {
    let valid = (1..=128).contains(&value.len())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b':' | b'-'));
    if valid {
        Ok(())
    } else {
        Err(AuditExportError::corrupt(
            "audit export policy identity is invalid",
        ))
    }
}

fn canonical_id(value: &str, prefix: &str) -> bool {
    value.strip_prefix(&format!("{prefix}_")).is_some_and(|suffix| {
        suffix.len() == 26
            && suffix.bytes().all(|byte| {
                byte.is_ascii_digit()
                    || matches!(byte, b'A'..=b'H' | b'J'..=b'K' | b'M'..=b'N' | b'P'..=b'T' | b'V'..=b'Z')
            })
    })
}

fn sha256_digest(bytes: &[u8]) -> Sha256Digest {
    Sha256Digest(format!("sha256:{:x}", Sha256::digest(bytes)))
}

fn framed(hash: &mut Sha256, value: &[u8]) {
    hash.update(u64::try_from(value.len()).unwrap_or(u64::MAX).to_be_bytes());
    hash.update(value);
}

fn frame_optional(hash: &mut Sha256, value: Option<&[u8]>) {
    match value {
        Some(value) => {
            hash.update([1]);
            framed(hash, value);
        }
        None => hash.update([0]),
    }
}

/// Stable export failure category.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AuditExportErrorKind {
    InvalidInput,
    SnapshotConflict,
    Corrupt,
    Store,
}

/// Secret-safe export failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuditExportError {
    kind: AuditExportErrorKind,
    message: String,
}

impl AuditExportError {
    fn invalid(message: impl Into<String>) -> Self {
        Self {
            kind: AuditExportErrorKind::InvalidInput,
            message: message.into(),
        }
    }

    fn snapshot(message: impl Into<String>) -> Self {
        Self {
            kind: AuditExportErrorKind::SnapshotConflict,
            message: message.into(),
        }
    }

    fn corrupt(message: impl Into<String>) -> Self {
        Self {
            kind: AuditExportErrorKind::Corrupt,
            message: message.into(),
        }
    }

    fn store(error: AuditError) -> Self {
        let kind = match error.kind() {
            AuditErrorKind::InvalidInput => AuditExportErrorKind::InvalidInput,
            AuditErrorKind::RequestConflict => AuditExportErrorKind::SnapshotConflict,
            AuditErrorKind::Corrupt => AuditExportErrorKind::Corrupt,
            AuditErrorKind::Adapter | AuditErrorKind::Closed => AuditExportErrorKind::Store,
        };
        drop(error);
        Self {
            kind,
            message: "canonical Audit Ledger export failed".to_owned(),
        }
    }

    fn encoding(error: serde_json::Error) -> Self {
        drop(error);
        Self {
            kind: AuditExportErrorKind::Corrupt,
            message: "audit export canonical encoding failed".to_owned(),
        }
    }

    #[must_use]
    pub const fn kind(&self) -> AuditExportErrorKind {
        self.kind
    }
}

impl fmt::Display for AuditExportError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for AuditExportError {}
