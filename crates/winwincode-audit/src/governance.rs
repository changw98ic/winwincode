// SPDX-License-Identifier: Apache-2.0

//! Fail-closed data-governance policy authority.
//!
//! This module evaluates immutable classification, residency, redaction,
//! retention, and legal-hold facts. It never receives raw content. A deletion
//! can reach storage only through a sealed [`DeletionPermit`] and the explicit
//! [`GovernedDeletionPort`]. The policy decision is appended to the canonical
//! [`AuditStore`] before that port is called.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use winwincode_domain::{RequestId, Sha256Digest};

use crate::{
    AuditAction, AuditActor, AuditError, AuditEvent, AuditEventId, AuditOrigin, AuditRetention,
    AuditScope, AuditState, AuditStore, AuditSubject,
};

const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;
const RULES_DOMAIN: &[u8] = b"winwincode.data-governance-rules.v1";
const DECISION_DOMAIN: &[u8] = b"winwincode.data-governance-decision.v1";
const AUDIT_DOMAIN: &[u8] = b"winwincode.data-governance-audit.v1";

/// Stable data sensitivity classes, ordered from least to most restricted.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DataClassification {
    Public,
    Internal,
    Confidential,
    Restricted,
    Secret,
}

impl DataClassification {
    const ALL: [Self; 5] = [
        Self::Public,
        Self::Internal,
        Self::Confidential,
        Self::Restricted,
        Self::Secret,
    ];

    const fn token(self) -> &'static str {
        match self {
            Self::Public => "public",
            Self::Internal => "internal",
            Self::Confidential => "confidential",
            Self::Restricted => "restricted",
            Self::Secret => "secret",
        }
    }
}

/// Canonical region tag used by residency rules.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub struct ResidencyRegion(String);

impl ResidencyRegion {
    /// Builds a bounded lower-case region tag such as `cn-north-1`.
    ///
    /// # Errors
    ///
    /// Rejects an empty, unbounded, or non-canonical tag.
    pub fn try_new(value: impl Into<String>) -> Result<Self, GovernanceError> {
        let value = value.into();
        let valid = (2..=64).contains(&value.len())
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
                .is_some_and(u8::is_ascii_alphanumeric);
        if !valid {
            return Err(GovernanceError::invalid(
                "data residency region is not canonical",
            ));
        }
        Ok(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The only content transformations that a governance rule can authorize.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RedactionStrategy {
    Reveal,
    Mask,
    Hash,
    Remove,
}

impl RedactionStrategy {
    const fn token(self) -> &'static str {
        match self {
            Self::Reveal => "reveal",
            Self::Mask => "mask",
            Self::Hash => "hash",
            Self::Remove => "remove",
        }
    }
}

/// Minimum retention attached to one classification.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "duration_millis", rename_all = "snake_case")]
pub enum RetentionRequirement {
    MinimumDuration(u64),
    Indefinite,
}

/// One complete rule for a sensitivity class.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClassificationRule {
    classification: DataClassification,
    allowed_regions: BTreeSet<ResidencyRegion>,
    retention: RetentionRequirement,
    redaction: RedactionStrategy,
}

impl ClassificationRule {
    /// Builds a rule with an explicit non-empty region allow-list.
    ///
    /// # Errors
    ///
    /// Rejects duplicate/empty regions, out-of-range retention, or revealing
    /// confidential, restricted, or secret content.
    pub fn try_new(
        classification: DataClassification,
        allowed_regions: impl IntoIterator<Item = ResidencyRegion>,
        retention: RetentionRequirement,
        redaction: RedactionStrategy,
    ) -> Result<Self, GovernanceError> {
        let allowed_regions = allowed_regions.into_iter().collect::<BTreeSet<_>>();
        if allowed_regions.is_empty() {
            return Err(GovernanceError::invalid(
                "classification rule requires at least one allowed region",
            ));
        }
        if matches!(retention, RetentionRequirement::MinimumDuration(value) if value > MAX_SAFE_INTEGER)
        {
            return Err(GovernanceError::invalid(
                "classification retention exceeds the exact timestamp range",
            ));
        }
        if classification >= DataClassification::Confidential
            && redaction == RedactionStrategy::Reveal
        {
            return Err(GovernanceError::invalid(
                "confidential, restricted, and secret content cannot use reveal redaction",
            ));
        }
        Ok(Self {
            classification,
            allowed_regions,
            retention,
            redaction,
        })
    }

    #[must_use]
    pub const fn classification(&self) -> DataClassification {
        self.classification
    }

    #[must_use]
    pub fn allowed_regions(&self) -> &BTreeSet<ResidencyRegion> {
        &self.allowed_regions
    }

    #[must_use]
    pub const fn retention(&self) -> RetentionRequirement {
        self.retention
    }

    #[must_use]
    pub const fn redaction(&self) -> RedactionStrategy {
        self.redaction
    }
}

/// Stable legal-hold identity.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub struct LegalHoldId(String);

impl LegalHoldId {
    /// Builds `lgh_` plus 26 Crockford characters.
    ///
    /// # Errors
    ///
    /// Rejects a non-canonical identity.
    pub fn try_new(value: impl Into<String>) -> Result<Self, GovernanceError> {
        let value = value.into();
        if !canonical_id(&value, "lgh") {
            return Err(GovernanceError::invalid(
                "legal hold identity is not canonical",
            ));
        }
        Ok(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Immutable legal-hold fact. An absent source digest applies to every data
/// object under the hold scope.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LegalHold {
    id: LegalHoldId,
    scope: AuditScope,
    source_digest: Option<Sha256Digest>,
    effective_at_millis: u64,
    released_at_millis: Option<u64>,
}

impl LegalHold {
    /// Builds a scoped legal hold.
    ///
    /// # Errors
    ///
    /// Rejects malformed digests/timestamps or release at/before activation.
    pub fn try_new(
        id: LegalHoldId,
        scope: AuditScope,
        source_digest: Option<Sha256Digest>,
        effective_at_millis: u64,
        released_at_millis: Option<u64>,
    ) -> Result<Self, GovernanceError> {
        validate_scope(&scope)?;
        if let Some(digest) = &source_digest {
            validate_digest(digest, "legal hold source digest")?;
        }
        validate_time(effective_at_millis, "legal hold effective time")?;
        if released_at_millis
            .is_some_and(|released| released <= effective_at_millis || released > MAX_SAFE_INTEGER)
        {
            return Err(GovernanceError::invalid(
                "legal hold release must follow activation within the exact timestamp range",
            ));
        }
        Ok(Self {
            id,
            scope,
            source_digest,
            effective_at_millis,
            released_at_millis,
        })
    }

    #[must_use]
    pub const fn id(&self) -> &LegalHoldId {
        &self.id
    }

    fn applies_to(&self, data: &GovernedDataFact, as_of_millis: u64) -> bool {
        self.effective_at_millis <= as_of_millis
            && self
                .released_at_millis
                .is_none_or(|released| as_of_millis < released)
            && scope_contains(&self.scope, data.scope())
            && self
                .source_digest
                .as_ref()
                .is_none_or(|digest| digest == data.source_digest())
    }
}

/// Secret-free immutable facts used by every governance decision.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GovernedDataFact {
    scope: AuditScope,
    source_digest: Sha256Digest,
    classification: DataClassification,
    resident_region: ResidencyRegion,
    created_at_millis: u64,
}

impl GovernedDataFact {
    /// Builds one governed data fact without accepting raw content.
    ///
    /// # Errors
    ///
    /// Rejects malformed scope, digest, or timestamp facts.
    pub fn try_new(
        scope: AuditScope,
        source_digest: Sha256Digest,
        classification: DataClassification,
        resident_region: ResidencyRegion,
        created_at_millis: u64,
    ) -> Result<Self, GovernanceError> {
        validate_scope(&scope)?;
        validate_digest(&source_digest, "governed data source digest")?;
        validate_time(created_at_millis, "governed data creation time")?;
        Ok(Self {
            scope,
            source_digest,
            classification,
            resident_region,
            created_at_millis,
        })
    }

    #[must_use]
    pub const fn scope(&self) -> &AuditScope {
        &self.scope
    }

    #[must_use]
    pub const fn source_digest(&self) -> &Sha256Digest {
        &self.source_digest
    }

    #[must_use]
    pub const fn classification(&self) -> DataClassification {
        self.classification
    }

    #[must_use]
    pub const fn resident_region(&self) -> &ResidencyRegion {
        &self.resident_region
    }
}

/// Traceable retention plan for one exact source fact.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RetentionPlan {
    source_digest: Sha256Digest,
    requirement: RetentionRequirement,
    delete_not_before_millis: Option<u64>,
    active_legal_holds: Vec<LegalHoldId>,
    rule_id: String,
    rule_version: u64,
    rule_digest: Sha256Digest,
}

impl RetentionPlan {
    #[must_use]
    pub const fn requirement(&self) -> RetentionRequirement {
        self.requirement
    }

    #[must_use]
    pub const fn delete_not_before_millis(&self) -> Option<u64> {
        self.delete_not_before_millis
    }

    #[must_use]
    pub fn active_legal_holds(&self) -> &[LegalHoldId] {
        &self.active_legal_holds
    }

    #[must_use]
    pub const fn source_digest(&self) -> &Sha256Digest {
        &self.source_digest
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

/// Secret-free redaction decision that retains the original source digest.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RedactionPlan {
    scope: AuditScope,
    source_digest: Sha256Digest,
    classification: DataClassification,
    resident_region: ResidencyRegion,
    evaluated_at_millis: u64,
    strategy: RedactionStrategy,
    decision_digest: Sha256Digest,
    rule_id: String,
    rule_version: u64,
    rule_digest: Sha256Digest,
}

impl RedactionPlan {
    #[must_use]
    pub const fn scope(&self) -> &AuditScope {
        &self.scope
    }

    #[must_use]
    pub const fn source_digest(&self) -> &Sha256Digest {
        &self.source_digest
    }

    #[must_use]
    pub const fn classification(&self) -> DataClassification {
        self.classification
    }

    #[must_use]
    pub const fn resident_region(&self) -> &ResidencyRegion {
        &self.resident_region
    }

    #[must_use]
    pub const fn evaluated_at_millis(&self) -> u64 {
        self.evaluated_at_millis
    }

    #[must_use]
    pub const fn strategy(&self) -> RedactionStrategy {
        self.strategy
    }

    #[must_use]
    pub const fn decision_digest(&self) -> &Sha256Digest {
        &self.decision_digest
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

/// Stable fail-closed policy denial.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GovernanceDenial {
    ResidencyDenied,
    RetentionActive { delete_not_before_millis: u64 },
    IndefiniteRetention,
    LegalHoldActive { legal_hold_id: LegalHoldId },
}

impl GovernanceDenial {
    const fn result_code(&self) -> &'static str {
        match self {
            Self::ResidencyDenied => "residency-denied",
            Self::RetentionActive { .. } => "retention-active",
            Self::IndefiniteRetention => "retention-indefinite",
            Self::LegalHoldActive { .. } => "legal-hold-active",
        }
    }
}

/// Sealed residency authorization for one exact source and destination.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlacementPermit {
    source_digest: Sha256Digest,
    destination_region: ResidencyRegion,
    decision_digest: Sha256Digest,
    rule_version: u64,
    rule_digest: Sha256Digest,
}

impl PlacementPermit {
    #[must_use]
    pub const fn source_digest(&self) -> &Sha256Digest {
        &self.source_digest
    }

    #[must_use]
    pub const fn destination_region(&self) -> &ResidencyRegion {
        &self.destination_region
    }

    #[must_use]
    pub const fn decision_digest(&self) -> &Sha256Digest {
        &self.decision_digest
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

/// Residency evaluation result.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PlacementDecision {
    Allowed(PlacementPermit),
    Denied {
        denial: GovernanceDenial,
        decision_digest: Sha256Digest,
    },
}

impl PlacementDecision {
    #[must_use]
    pub const fn decision_digest(&self) -> &Sha256Digest {
        match self {
            Self::Allowed(permit) => permit.decision_digest(),
            Self::Denied {
                decision_digest, ..
            } => decision_digest,
        }
    }
}

/// Sealed authorization consumed by the only deletion port.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeletionPermit {
    scope: AuditScope,
    source_digest: Sha256Digest,
    requested_at_millis: u64,
    decision_digest: Sha256Digest,
    rule_version: u64,
    rule_digest: Sha256Digest,
}

impl DeletionPermit {
    #[must_use]
    pub const fn scope(&self) -> &AuditScope {
        &self.scope
    }

    #[must_use]
    pub const fn source_digest(&self) -> &Sha256Digest {
        &self.source_digest
    }

    #[must_use]
    pub const fn requested_at_millis(&self) -> u64 {
        self.requested_at_millis
    }

    #[must_use]
    pub const fn decision_digest(&self) -> &Sha256Digest {
        &self.decision_digest
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

/// Deletion evaluation result.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DeletionDecision {
    Allowed(DeletionPermit),
    Denied {
        denial: GovernanceDenial,
        decision_digest: Sha256Digest,
        requested_at_millis: u64,
    },
}

impl DeletionDecision {
    #[must_use]
    pub const fn decision_digest(&self) -> &Sha256Digest {
        match self {
            Self::Allowed(permit) => permit.decision_digest(),
            Self::Denied {
                decision_digest, ..
            } => decision_digest,
        }
    }

    #[must_use]
    pub const fn denial(&self) -> Option<&GovernanceDenial> {
        match self {
            Self::Allowed(_) => None,
            Self::Denied { denial, .. } => Some(denial),
        }
    }
}

/// Immutable context used to record one policy decision before deletion.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GovernanceAuditContext {
    actor: AuditActor,
    request_id: RequestId,
    origin: AuditOrigin,
}

impl GovernanceAuditContext {
    #[must_use]
    pub const fn new(actor: AuditActor, request_id: RequestId, origin: AuditOrigin) -> Self {
        Self {
            actor,
            request_id,
            origin,
        }
    }
}

/// Storage boundary for a deletion authorized by this module. Implementations
/// must be idempotent by `decision_digest`.
pub trait GovernedDeletionPort {
    /// Applies one sealed deletion permit.
    ///
    /// # Errors
    ///
    /// Returns an adapter-neutral error without remote or sensitive text.
    fn delete(&mut self, permit: &DeletionPermit)
    -> Result<DeletionPortOutcome, DeletionPortError>;
}

/// Idempotent deletion result.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeletionPortOutcome {
    Deleted,
    AlreadyDeleted,
}

/// Stable deletion adapter failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeletionPortError;

impl DeletionPortError {
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl Default for DeletionPortError {
    fn default() -> Self {
        Self::new()
    }
}

/// Result of the audited policy-to-deletion-port coordinator.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GovernedDeletionResult {
    Denied(GovernanceDenial),
    Applied {
        permit: DeletionPermit,
        outcome: DeletionPortOutcome,
    },
}

/// Immutable policy authority for classification, residency, redaction,
/// retention, and legal holds.
#[derive(Debug)]
pub struct DataGovernanceAuthority {
    rule_id: String,
    rule_version: u64,
    rule_digest: Sha256Digest,
    rules: BTreeMap<DataClassification, ClassificationRule>,
    legal_holds: Vec<LegalHold>,
}

impl DataGovernanceAuthority {
    /// Builds one complete policy snapshot.
    ///
    /// # Errors
    ///
    /// Requires one and only one rule for every classification, a canonical
    /// rule identity/version/digest, and unique legal-hold identities.
    pub fn try_new(
        rule_id: &str,
        rule_version: u64,
        rules: impl IntoIterator<Item = ClassificationRule>,
        legal_holds: impl IntoIterator<Item = LegalHold>,
    ) -> Result<Self, GovernanceError> {
        validate_token(rule_id, "data governance rule identity")?;
        if rule_version == 0 || rule_version > MAX_SAFE_INTEGER {
            return Err(GovernanceError::invalid(
                "data governance rule version is out of range",
            ));
        }
        if format!("{rule_id}.v{rule_version}").len() > 128 {
            return Err(GovernanceError::invalid(
                "versioned data governance rule identity is too long",
            ));
        }
        let mut indexed = BTreeMap::new();
        for rule in rules {
            if indexed.insert(rule.classification(), rule).is_some() {
                return Err(GovernanceError::invalid(
                    "data governance classifications must be unique",
                ));
            }
        }
        if DataClassification::ALL
            .iter()
            .any(|classification| !indexed.contains_key(classification))
        {
            return Err(GovernanceError::invalid(
                "data governance policy must cover every classification",
            ));
        }
        let mut legal_holds = legal_holds.into_iter().collect::<Vec<_>>();
        let mut ids = BTreeSet::new();
        if legal_holds
            .iter()
            .any(|hold| !ids.insert(hold.id().clone()))
        {
            return Err(GovernanceError::invalid(
                "legal hold identities must be unique",
            ));
        }
        legal_holds.sort_by(|left, right| left.id().cmp(right.id()));
        let rule_digest = rules_digest(rule_id, rule_version, &indexed, &legal_holds);
        Ok(Self {
            rule_id: rule_id.to_owned(),
            rule_version,
            rule_digest,
            rules: indexed,
            legal_holds,
        })
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

    /// Computes the traceable retention and legal-hold plan.
    ///
    /// # Errors
    ///
    /// Fails closed when the source already resides outside its allow-list or
    /// a retention timestamp overflows the exact range.
    pub fn retention_plan(
        &self,
        data: &GovernedDataFact,
        as_of_millis: u64,
    ) -> Result<RetentionPlan, GovernanceError> {
        validate_time(as_of_millis, "retention evaluation time")?;
        let rule = self.rule_for(data)?;
        Self::validate_current_residency(data, rule)?;
        let delete_not_before_millis = match rule.retention() {
            RetentionRequirement::MinimumDuration(duration) => Some(
                data.created_at_millis
                    .checked_add(duration)
                    .filter(|value| *value <= MAX_SAFE_INTEGER)
                    .ok_or_else(|| {
                        GovernanceError::invalid("retention deadline exceeds the exact range")
                    })?,
            ),
            RetentionRequirement::Indefinite => None,
        };
        let active_legal_holds = self
            .legal_holds
            .iter()
            .filter(|hold| hold.applies_to(data, as_of_millis))
            .map(|hold| hold.id().clone())
            .collect();
        Ok(RetentionPlan {
            source_digest: data.source_digest.clone(),
            requirement: rule.retention(),
            delete_not_before_millis,
            active_legal_holds,
            rule_id: self.rule_id.clone(),
            rule_version: self.rule_version,
            rule_digest: self.rule_digest.clone(),
        })
    }

    /// Produces a provenance-preserving redaction plan.
    ///
    /// # Errors
    ///
    /// Fails closed when the current residency fact violates the rule set.
    pub fn redaction_plan(
        &self,
        data: &GovernedDataFact,
    ) -> Result<RedactionPlan, GovernanceError> {
        let rule = self.rule_for(data)?;
        Self::validate_current_residency(data, rule)?;
        let decision_digest = self.decision_digest(
            data,
            "redaction",
            rule.redaction().token(),
            data.created_at_millis,
        );
        Ok(RedactionPlan {
            scope: data.scope.clone(),
            source_digest: data.source_digest.clone(),
            classification: data.classification,
            resident_region: data.resident_region.clone(),
            evaluated_at_millis: data.created_at_millis,
            strategy: rule.redaction(),
            decision_digest,
            rule_id: self.rule_id.clone(),
            rule_version: self.rule_version,
            rule_digest: self.rule_digest.clone(),
        })
    }

    /// Computes a redaction plan and appends its source/rule-bound decision to
    /// the canonical Audit Ledger. Exact request replay returns the same plan
    /// and does not add another audit sequence.
    ///
    /// # Errors
    ///
    /// Returns before producing a plan when residency is invalid, or when the
    /// Audit Ledger cannot durably record the decision.
    pub fn record_redaction(
        &self,
        data: &GovernedDataFact,
        occurred_at_millis: u64,
        audit_context: &GovernanceAuditContext,
        audit_store: &mut AuditStore,
    ) -> Result<RedactionPlan, GovernanceError> {
        validate_time(occurred_at_millis, "redaction audit time")?;
        let plan = self.redaction_plan(data)?;
        let event_digest = audit_identity_digest(plan.decision_digest(), audit_context)?;
        let event_id = AuditEventId::from_digest(&event_digest).map_err(GovernanceError::audit)?;
        let action = AuditAction::policy(&format!("{}.v{}", self.rule_id, self.rule_version))
            .map_err(GovernanceError::audit)?;
        let state = AuditState::unchanged(Some(plan.decision_digest().clone()))
            .map_err(GovernanceError::audit)?;
        let event = AuditEvent::succeeded(
            event_id,
            occurred_at_millis,
            audit_context.actor.clone(),
            data.scope.clone(),
            audit_context.request_id.clone(),
            action,
            state,
            audit_context.origin.clone(),
            AuditSubject::new(),
            "redaction-planned",
            AuditRetention::Indefinite,
        )
        .map_err(GovernanceError::audit)?;
        audit_store.append(&event).map_err(GovernanceError::audit)?;
        Ok(plan)
    }

    /// Authorizes placement only when both current and destination regions are
    /// allowed for the exact classification.
    ///
    /// # Errors
    ///
    /// Returns invalid/corrupt policy facts separately from an ordinary
    /// residency denial.
    pub fn evaluate_placement(
        &self,
        data: &GovernedDataFact,
        destination: &ResidencyRegion,
        as_of_millis: u64,
    ) -> Result<PlacementDecision, GovernanceError> {
        validate_time(as_of_millis, "placement evaluation time")?;
        let rule = self.rule_for(data)?;
        Self::validate_current_residency(data, rule)?;
        let digest = self.decision_digest(data, "placement", destination.as_str(), as_of_millis);
        if !rule.allowed_regions().contains(destination) {
            return Ok(PlacementDecision::Denied {
                denial: GovernanceDenial::ResidencyDenied,
                decision_digest: digest,
            });
        }
        Ok(PlacementDecision::Allowed(PlacementPermit {
            source_digest: data.source_digest.clone(),
            destination_region: destination.clone(),
            decision_digest: digest,
            rule_version: self.rule_version,
            rule_digest: self.rule_digest.clone(),
        }))
    }

    /// Evaluates retention and every applicable legal hold before minting the
    /// only deletion permit.
    ///
    /// # Errors
    ///
    /// Fails closed for malformed time or already-invalid residency facts.
    pub fn evaluate_deletion(
        &self,
        data: &GovernedDataFact,
        requested_at_millis: u64,
    ) -> Result<DeletionDecision, GovernanceError> {
        let plan = self.retention_plan(data, requested_at_millis)?;
        let denial = if let Some(hold) = plan.active_legal_holds().first() {
            Some(GovernanceDenial::LegalHoldActive {
                legal_hold_id: hold.clone(),
            })
        } else {
            match plan.requirement() {
                RetentionRequirement::Indefinite => Some(GovernanceDenial::IndefiniteRetention),
                RetentionRequirement::MinimumDuration(_) => plan
                    .delete_not_before_millis()
                    .filter(|deadline| requested_at_millis < *deadline)
                    .map(|deadline| GovernanceDenial::RetentionActive {
                        delete_not_before_millis: deadline,
                    }),
            }
        };
        let outcome = denial
            .as_ref()
            .map_or("allowed", GovernanceDenial::result_code);
        let decision_digest = self.decision_digest(data, "deletion", outcome, requested_at_millis);
        if let Some(denial) = denial {
            return Ok(DeletionDecision::Denied {
                denial,
                decision_digest,
                requested_at_millis,
            });
        }
        Ok(DeletionDecision::Allowed(DeletionPermit {
            scope: data.scope.clone(),
            source_digest: data.source_digest.clone(),
            requested_at_millis,
            decision_digest,
            rule_version: self.rule_version,
            rule_digest: self.rule_digest.clone(),
        }))
    }

    /// Records the deterministic policy decision in the canonical Audit
    /// Ledger before invoking the explicit storage deletion port.
    ///
    /// # Errors
    ///
    /// Returns before the port on invalid facts or an Audit Ledger error. A
    /// deletion adapter failure is returned without exposing adapter text.
    pub fn execute_deletion(
        &self,
        data: &GovernedDataFact,
        requested_at_millis: u64,
        audit_context: &GovernanceAuditContext,
        audit_store: &mut AuditStore,
        port: &mut dyn GovernedDeletionPort,
    ) -> Result<GovernedDeletionResult, GovernanceError> {
        let decision = self.evaluate_deletion(data, requested_at_millis)?;
        let audit_event = self.deletion_audit_event(data, &decision, audit_context)?;
        audit_store
            .append(&audit_event)
            .map_err(GovernanceError::audit)?;
        match decision {
            DeletionDecision::Denied { denial, .. } => Ok(GovernedDeletionResult::Denied(denial)),
            DeletionDecision::Allowed(permit) => {
                let outcome = port
                    .delete(&permit)
                    .map_err(|_| GovernanceError::adapter())?;
                Ok(GovernedDeletionResult::Applied { permit, outcome })
            }
        }
    }

    fn deletion_audit_event(
        &self,
        data: &GovernedDataFact,
        decision: &DeletionDecision,
        context: &GovernanceAuditContext,
    ) -> Result<AuditEvent, GovernanceError> {
        let event_digest = audit_identity_digest(decision.decision_digest(), context)?;
        let event_id = AuditEventId::from_digest(&event_digest).map_err(GovernanceError::audit)?;
        let action_name = format!("{}.v{}", self.rule_id, self.rule_version);
        let action = AuditAction::policy(&action_name).map_err(GovernanceError::audit)?;
        let state = AuditState::unchanged(Some(decision.decision_digest().clone()))
            .map_err(GovernanceError::audit)?;
        match decision {
            DeletionDecision::Allowed(_) => AuditEvent::succeeded(
                event_id,
                requested_time(decision),
                context.actor.clone(),
                data.scope.clone(),
                context.request_id.clone(),
                action,
                state,
                context.origin.clone(),
                AuditSubject::new(),
                "deletion-authorized",
                AuditRetention::Indefinite,
            ),
            DeletionDecision::Denied { denial, .. } => AuditEvent::rejected(
                event_id,
                requested_time(decision),
                context.actor.clone(),
                data.scope.clone(),
                context.request_id.clone(),
                action,
                state,
                context.origin.clone(),
                AuditSubject::new(),
                denial.result_code(),
                AuditRetention::Indefinite,
            ),
        }
        .map_err(GovernanceError::audit)
    }

    fn rule_for(&self, data: &GovernedDataFact) -> Result<&ClassificationRule, GovernanceError> {
        self.rules
            .get(&data.classification())
            .ok_or_else(|| GovernanceError::corrupt("classification rule is missing"))
    }

    fn validate_current_residency(
        data: &GovernedDataFact,
        rule: &ClassificationRule,
    ) -> Result<(), GovernanceError> {
        if rule.allowed_regions().contains(data.resident_region()) {
            Ok(())
        } else {
            Err(GovernanceError::residency())
        }
    }

    fn decision_digest(
        &self,
        data: &GovernedDataFact,
        operation: &str,
        outcome: &str,
        at_millis: u64,
    ) -> Sha256Digest {
        let mut hash = Sha256::new();
        framed(&mut hash, DECISION_DOMAIN);
        framed(&mut hash, self.rule_id.as_bytes());
        hash.update(self.rule_version.to_be_bytes());
        framed(&mut hash, self.rule_digest.0.as_bytes());
        framed(&mut hash, data.scope.organization_id().0.as_bytes());
        frame_optional(
            &mut hash,
            data.scope.workspace_id().map(|value| value.0.as_bytes()),
        );
        frame_optional(
            &mut hash,
            data.scope.project_id().map(|value| value.0.as_bytes()),
        );
        frame_optional(
            &mut hash,
            data.scope.repository_id().map(|value| value.0.as_bytes()),
        );
        framed(&mut hash, data.source_digest.0.as_bytes());
        framed(&mut hash, data.classification.token().as_bytes());
        framed(&mut hash, data.resident_region.0.as_bytes());
        framed(&mut hash, operation.as_bytes());
        framed(&mut hash, outcome.as_bytes());
        hash.update(at_millis.to_be_bytes());
        Sha256Digest(format!("sha256:{:x}", hash.finalize()))
    }
}

fn requested_time(decision: &DeletionDecision) -> u64 {
    match decision {
        DeletionDecision::Allowed(permit) => permit.requested_at_millis(),
        DeletionDecision::Denied {
            requested_at_millis,
            ..
        } => *requested_at_millis,
    }
}

fn audit_identity_digest(
    decision_digest: &Sha256Digest,
    context: &GovernanceAuditContext,
) -> Result<Sha256Digest, GovernanceError> {
    let actor = serde_json::to_vec(&context.actor)
        .map_err(|_| GovernanceError::invalid("audit actor cannot be encoded"))?;
    let origin = serde_json::to_vec(&context.origin)
        .map_err(|_| GovernanceError::invalid("audit origin cannot be encoded"))?;
    let mut hash = Sha256::new();
    framed(&mut hash, AUDIT_DOMAIN);
    framed(&mut hash, decision_digest.0.as_bytes());
    framed(&mut hash, context.request_id.0.as_bytes());
    framed(&mut hash, &actor);
    framed(&mut hash, &origin);
    Ok(Sha256Digest(format!("sha256:{:x}", hash.finalize())))
}

fn rules_digest(
    rule_id: &str,
    rule_version: u64,
    rules: &BTreeMap<DataClassification, ClassificationRule>,
    legal_holds: &[LegalHold],
) -> Sha256Digest {
    let mut hash = Sha256::new();
    framed(&mut hash, RULES_DOMAIN);
    framed(&mut hash, rule_id.as_bytes());
    hash.update(rule_version.to_be_bytes());
    for rule in rules.values() {
        framed(&mut hash, rule.classification.token().as_bytes());
        for region in rule.allowed_regions() {
            framed(&mut hash, region.as_str().as_bytes());
        }
        match rule.retention() {
            RetentionRequirement::MinimumDuration(duration) => {
                framed(&mut hash, b"minimum_duration");
                hash.update(duration.to_be_bytes());
            }
            RetentionRequirement::Indefinite => framed(&mut hash, b"indefinite"),
        }
        framed(&mut hash, rule.redaction().token().as_bytes());
    }
    for hold in legal_holds {
        framed(&mut hash, hold.id.0.as_bytes());
        framed(&mut hash, hold.scope.organization_id().0.as_bytes());
        frame_optional(
            &mut hash,
            hold.scope.workspace_id().map(|value| value.0.as_bytes()),
        );
        frame_optional(
            &mut hash,
            hold.scope.project_id().map(|value| value.0.as_bytes()),
        );
        frame_optional(
            &mut hash,
            hold.scope.repository_id().map(|value| value.0.as_bytes()),
        );
        frame_optional(
            &mut hash,
            hold.source_digest.as_ref().map(|value| value.0.as_bytes()),
        );
        hash.update(hold.effective_at_millis.to_be_bytes());
        match hold.released_at_millis {
            Some(released) => {
                hash.update([1]);
                hash.update(released.to_be_bytes());
            }
            None => hash.update([0]),
        }
    }
    Sha256Digest(format!("sha256:{:x}", hash.finalize()))
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

fn validate_scope(scope: &AuditScope) -> Result<(), GovernanceError> {
    scope
        .validate()
        .map_err(|_| GovernanceError::invalid("data governance scope is invalid"))
}

fn validate_time(value: u64, field: &str) -> Result<(), GovernanceError> {
    if value == 0 || value > MAX_SAFE_INTEGER {
        Err(GovernanceError::invalid(format!(
            "{field} is outside the exact timestamp range"
        )))
    } else {
        Ok(())
    }
}

fn validate_digest(digest: &Sha256Digest, field: &str) -> Result<(), GovernanceError> {
    let valid = digest.0.strip_prefix("sha256:").is_some_and(|value| {
        value.len() == 64
            && value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    });
    if valid {
        Ok(())
    } else {
        Err(GovernanceError::invalid(format!(
            "{field} is not a canonical SHA-256 digest"
        )))
    }
}

fn validate_token(value: &str, field: &str) -> Result<(), GovernanceError> {
    let valid = (1..=128).contains(&value.len())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b':' | b'-'));
    if valid {
        Ok(())
    } else {
        Err(GovernanceError::invalid(format!(
            "{field} is not a stable token"
        )))
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

fn framed(hash: &mut Sha256, value: &[u8]) {
    hash.update(u64::try_from(value.len()).unwrap_or(u64::MAX).to_be_bytes());
    hash.update(value);
}

/// Stable data-governance failure category.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GovernanceErrorKind {
    InvalidInput,
    ResidencyDenied,
    Corrupt,
    Audit,
    Adapter,
}

/// Secret-safe data-governance failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GovernanceError {
    kind: GovernanceErrorKind,
    message: String,
}

impl GovernanceError {
    fn invalid(message: impl Into<String>) -> Self {
        Self {
            kind: GovernanceErrorKind::InvalidInput,
            message: message.into(),
        }
    }

    fn residency() -> Self {
        Self {
            kind: GovernanceErrorKind::ResidencyDenied,
            message: "current data residency violates the governing classification rule".into(),
        }
    }

    fn corrupt(message: impl Into<String>) -> Self {
        Self {
            kind: GovernanceErrorKind::Corrupt,
            message: message.into(),
        }
    }

    fn audit(error: AuditError) -> Self {
        drop(error);
        Self {
            kind: GovernanceErrorKind::Audit,
            message: "data governance Audit Ledger operation failed".into(),
        }
    }

    fn adapter() -> Self {
        Self {
            kind: GovernanceErrorKind::Adapter,
            message: "governed deletion adapter failed".into(),
        }
    }

    #[must_use]
    pub const fn kind(&self) -> GovernanceErrorKind {
        self.kind
    }
}

impl fmt::Display for GovernanceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for GovernanceError {}
