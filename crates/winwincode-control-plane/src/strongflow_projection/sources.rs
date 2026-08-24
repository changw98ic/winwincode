// SPDX-License-Identifier: Apache-2.0

//! Trusted source ports and their bounded, validated read values.

use std::{error::Error, fmt};

use serde::Serialize;
use winwincode_api::generated::RepositoryScope;
use winwincode_delivery::{
    domain::{DeliverySpecId, DeliveryVerdictId, FrozenDeliveryCandidate},
    projection::runtime::{RuntimeFoldSnapshot, RuntimeProjection},
};
use winwincode_domain::{
    AttentionItemId, DeliveryId, Instant, ProductSessionId, Revision, Sha256Digest,
};

const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

/// Expected accepted-ledger coordinates when replaying a server-issued cut.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeCutExpectation {
    ledger_revision: Revision,
    accepted_sequence: u64,
}

impl RuntimeCutExpectation {
    #[must_use]
    pub const fn ledger_revision(&self) -> &Revision {
        &self.ledger_revision
    }

    #[must_use]
    pub const fn accepted_sequence(&self) -> u64 {
        self.accepted_sequence
    }

    pub(crate) const fn new(ledger_revision: Revision, accepted_sequence: u64) -> Self {
        Self {
            ledger_revision,
            accepted_sequence,
        }
    }
}

/// Exact bounded runtime-ledger request created only by the application service.
#[derive(Debug, Clone, PartialEq)]
pub struct DeliveryRuntimeReadRequest {
    scope: RepositoryScope,
    delivery_id: DeliveryId,
    delivery_revision: u64,
    expected: Option<RuntimeCutExpectation>,
    limit: usize,
}

impl DeliveryRuntimeReadRequest {
    #[must_use]
    pub const fn scope(&self) -> &RepositoryScope {
        &self.scope
    }

    #[must_use]
    pub const fn delivery_id(&self) -> &DeliveryId {
        &self.delivery_id
    }

    #[must_use]
    pub const fn delivery_revision(&self) -> u64 {
        self.delivery_revision
    }

    #[must_use]
    pub const fn expected(&self) -> Option<&RuntimeCutExpectation> {
        self.expected.as_ref()
    }

    #[must_use]
    pub const fn limit(&self) -> usize {
        self.limit
    }

    pub(crate) fn new(
        scope: RepositoryScope,
        delivery_id: DeliveryId,
        delivery_revision: u64,
        expected: Option<RuntimeCutExpectation>,
        limit: usize,
    ) -> Self {
        Self {
            scope,
            delivery_id,
            delivery_revision,
            expected,
            limit,
        }
    }
}

/// Adapter-neutral failure from a trusted source owner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrustedProjectionReadError {
    Unavailable,
    TemporarilyUnavailable,
    Stale,
    Invalid,
}

impl fmt::Display for TrustedProjectionReadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Unavailable => "trusted facts are unavailable",
            Self::TemporarilyUnavailable => "trusted fact storage is temporarily unavailable",
            Self::Stale => "trusted facts no longer name the requested cut",
            Self::Invalid => "trusted facts are not canonical",
        })
    }
}

impl Error for TrustedProjectionReadError {}

/// One verified accepted-ledger fold plus its durable cut coordinates.
#[derive(Debug, Clone)]
pub struct TrustedRuntimeProjectionRead {
    delivery_revision: u64,
    ledger_revision: Revision,
    accepted_sequence: u64,
    rebuilt_at: Instant,
    projection: RuntimeProjection,
    source_seal: Sha256Digest,
}

impl TrustedRuntimeProjectionRead {
    /// Validates one projection already rebuilt by the accepted-ledger owner.
    ///
    /// # Errors
    ///
    /// Rejects non-positive revisions, incomplete sequence coordinates, an
    /// oversized fold, unsafe timestamps, or a malformed durable source seal.
    pub fn try_new(
        delivery_revision: u64,
        ledger_revision: Revision,
        accepted_sequence: u64,
        rebuilt_at: Instant,
        projection: RuntimeProjection,
        source_seal: Sha256Digest,
    ) -> Result<Self, TrustedProjectionReadError> {
        let snapshot = projection.snapshot();
        let max_session_sequence = snapshot
            .sessions
            .iter()
            .map(|session| session.as_of_sequence)
            .max()
            .unwrap_or(0);
        if delivery_revision == 0
            || delivery_revision > MAX_SAFE_INTEGER
            || ledger_revision.0 < 0
            || accepted_sequence > MAX_SAFE_INTEGER
            || accepted_sequence < max_session_sequence
            || snapshot.sessions.len() > 256
            || !canonical_instant(&rebuilt_at)
            || !canonical_sha256(&source_seal)
        {
            return Err(TrustedProjectionReadError::Invalid);
        }
        Ok(Self {
            delivery_revision,
            ledger_revision,
            accepted_sequence,
            rebuilt_at,
            projection,
            source_seal,
        })
    }

    #[must_use]
    pub const fn delivery_revision(&self) -> u64 {
        self.delivery_revision
    }

    #[must_use]
    pub const fn ledger_revision(&self) -> &Revision {
        &self.ledger_revision
    }

    #[must_use]
    pub const fn accepted_sequence(&self) -> u64 {
        self.accepted_sequence
    }

    #[must_use]
    pub const fn rebuilt_at(&self) -> &Instant {
        &self.rebuilt_at
    }

    #[must_use]
    pub fn snapshot(&self) -> &RuntimeFoldSnapshot {
        self.projection.snapshot()
    }

    pub(crate) const fn source_seal(&self) -> &Sha256Digest {
        &self.source_seal
    }
}

/// Trusted append-only runtime-ledger reader.
pub trait TrustedRuntimeProjectionAdapter: Send + Sync {
    /// Reads latest or exact accepted facts for one current aggregate scope.
    ///
    /// # Errors
    ///
    /// Returns a stable source failure without substituting live Worker input.
    fn read_delivery(
        &self,
        request: &DeliveryRuntimeReadRequest,
    ) -> Result<TrustedRuntimeProjectionRead, TrustedProjectionReadError>;

    /// Reads a product-session projection independently of any aggregate cursor.
    ///
    /// # Errors
    ///
    /// The default remains closed until the accepted-ledger adapter implements it.
    fn read_product_session(
        &self,
        _scope: &RepositoryScope,
        _product_session_id: &ProductSessionId,
        _limit: usize,
    ) -> Result<TrustedRuntimeProjectionRead, TrustedProjectionReadError> {
        Err(TrustedProjectionReadError::Unavailable)
    }
}

/// Immutable identity stored with one trusted publication intent/result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PublicationFactBinding {
    delivery_id: DeliveryId,
    delivery_revision: u64,
    delivery_spec_id: DeliverySpecId,
    delivery_spec_revision: u64,
    candidate_ref: String,
    diff_sha256: String,
    verdict_id: DeliveryVerdictId,
    approval_id: AttentionItemId,
    approval_review_set_sha256: String,
    target_sha256: String,
}

/// Closed remote resource identity accepted from the trusted publication owner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublicationResourceKind {
    GitHubIssue,
    GitHubPullRequest,
}

/// Secret-safe remote publication identity. It never contains a URL or payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicationResourceFact {
    kind: PublicationResourceKind,
    repository: String,
    number: u64,
}

impl PublicationResourceFact {
    /// Builds a closed GitHub resource identity.
    ///
    /// # Errors
    ///
    /// Rejects a malformed repository slug or unsafe issue/PR number.
    pub fn try_new(
        kind: PublicationResourceKind,
        repository: impl Into<String>,
        number: u64,
    ) -> Result<Self, TrustedProjectionReadError> {
        let fact = Self {
            kind,
            repository: repository.into(),
            number,
        };
        let mut segments = fact.repository.split('/');
        let canonical_repository = segments.next().is_some_and(|value| portable(value, 100))
            && segments.next().is_some_and(|value| portable(value, 100))
            && segments.next().is_none();
        if !canonical_repository || number == 0 || number > MAX_SAFE_INTEGER {
            return Err(TrustedProjectionReadError::Invalid);
        }
        Ok(fact)
    }

    #[must_use]
    pub const fn kind(&self) -> PublicationResourceKind {
        self.kind
    }

    #[must_use]
    pub fn repository(&self) -> &str {
        &self.repository
    }

    #[must_use]
    pub const fn number(&self) -> u64 {
        self.number
    }
}

impl PublicationFactBinding {
    /// Builds the complete immutable publication fact binding.
    ///
    /// # Errors
    ///
    /// Rejects incomplete identities, revisions, references, or digests.
    #[allow(clippy::too_many_arguments)]
    pub fn try_new(
        delivery_id: DeliveryId,
        delivery_revision: u64,
        delivery_spec_id: DeliverySpecId,
        delivery_spec_revision: u64,
        candidate_ref: impl Into<String>,
        diff_sha256: impl Into<String>,
        verdict_id: DeliveryVerdictId,
        approval_id: AttentionItemId,
        approval_review_set_sha256: impl Into<String>,
        target_sha256: impl Into<String>,
    ) -> Result<Self, TrustedProjectionReadError> {
        let fact = Self {
            delivery_id,
            delivery_revision,
            delivery_spec_id,
            delivery_spec_revision,
            candidate_ref: candidate_ref.into(),
            diff_sha256: diff_sha256.into(),
            verdict_id,
            approval_id,
            approval_review_set_sha256: approval_review_set_sha256.into(),
            target_sha256: target_sha256.into(),
        };
        if fact.delivery_revision == 0
            || fact.delivery_revision > MAX_SAFE_INTEGER
            || fact.delivery_spec_revision == 0
            || fact.delivery_spec_revision > MAX_SAFE_INTEGER
            || !portable(&fact.delivery_id.0, 200)
            || !portable(&fact.delivery_spec_id.0, 200)
            || !portable(&fact.verdict_id.0, 200)
            || !portable(&fact.approval_id.0, 200)
            || !portable(&fact.candidate_ref, 4_096)
            || !lowercase_sha256(&fact.diff_sha256)
            || !lowercase_sha256(&fact.approval_review_set_sha256)
            || !lowercase_sha256(&fact.target_sha256)
        {
            return Err(TrustedProjectionReadError::Invalid);
        }
        Ok(fact)
    }

    #[must_use]
    pub const fn delivery_id(&self) -> &DeliveryId {
        &self.delivery_id
    }

    #[must_use]
    pub const fn delivery_revision(&self) -> u64 {
        self.delivery_revision
    }

    #[must_use]
    pub const fn delivery_spec_id(&self) -> &DeliverySpecId {
        &self.delivery_spec_id
    }

    #[must_use]
    pub const fn delivery_spec_revision(&self) -> u64 {
        self.delivery_spec_revision
    }

    #[must_use]
    pub fn candidate_ref(&self) -> &str {
        &self.candidate_ref
    }

    #[must_use]
    pub fn diff_sha256(&self) -> &str {
        &self.diff_sha256
    }

    #[must_use]
    pub const fn verdict_id(&self) -> &DeliveryVerdictId {
        &self.verdict_id
    }

    #[must_use]
    pub const fn approval_id(&self) -> &AttentionItemId {
        &self.approval_id
    }

    #[must_use]
    pub fn approval_review_set_sha256(&self) -> &str {
        &self.approval_review_set_sha256
    }

    #[must_use]
    pub fn target_sha256(&self) -> &str {
        &self.target_sha256
    }
}

/// Safe publication result fields supplied by the publication owner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicationResultFact {
    publication_id: winwincode_domain::PublicationId,
    revision: Revision,
    state: String,
    updated_at: Instant,
    binding: PublicationFactBinding,
    resource: Option<PublicationResourceFact>,
}

impl PublicationResultFact {
    /// Builds one safe publication result bound to an authorized fact set.
    ///
    /// # Errors
    ///
    /// Rejects malformed identity, state, time, or resource facts.
    pub fn try_new(
        publication_id: winwincode_domain::PublicationId,
        revision: Revision,
        state: impl Into<String>,
        updated_at: Instant,
        binding: PublicationFactBinding,
        resource: Option<PublicationResourceFact>,
    ) -> Result<Self, TrustedProjectionReadError> {
        let fact = Self {
            publication_id,
            revision,
            state: state.into(),
            updated_at,
            binding,
            resource,
        };
        if !portable(&fact.publication_id.0, 200)
            || fact.revision.0 < 1
            || !matches!(
                fact.state.as_str(),
                "pending" | "publishing" | "published" | "failed" | "cancelled"
            )
            || !canonical_instant(&fact.updated_at)
        {
            return Err(TrustedProjectionReadError::Invalid);
        }
        Ok(fact)
    }

    #[must_use]
    pub const fn publication_id(&self) -> &winwincode_domain::PublicationId {
        &self.publication_id
    }

    #[must_use]
    pub const fn revision(&self) -> &Revision {
        &self.revision
    }

    #[must_use]
    pub fn state(&self) -> &str {
        &self.state
    }

    #[must_use]
    pub const fn updated_at(&self) -> &Instant {
        &self.updated_at
    }

    #[must_use]
    pub const fn binding(&self) -> &PublicationFactBinding {
        &self.binding
    }

    #[must_use]
    pub const fn resource(&self) -> Option<&PublicationResourceFact> {
        self.resource.as_ref()
    }
}

/// Candidate and publication facts read from one durable publication cut.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrustedPublicationProjectionRead {
    delivery_revision: u64,
    publication_revision: Revision,
    candidate: Option<FrozenDeliveryCandidate>,
    result: Option<PublicationResultFact>,
    source_seal: Sha256Digest,
}

impl TrustedPublicationProjectionRead {
    /// Builds one sealed publication-ledger read.
    ///
    /// # Errors
    ///
    /// Rejects unsafe revisions, source seals, or a mismatched result revision.
    pub fn try_new(
        delivery_revision: u64,
        publication_revision: Revision,
        candidate: Option<FrozenDeliveryCandidate>,
        result: Option<PublicationResultFact>,
        source_seal: Sha256Digest,
    ) -> Result<Self, TrustedProjectionReadError> {
        if delivery_revision == 0
            || delivery_revision > MAX_SAFE_INTEGER
            || publication_revision.0 < 0
            || !canonical_sha256(&source_seal)
            || result
                .as_ref()
                .is_some_and(|result| result.revision() != &publication_revision)
        {
            return Err(TrustedProjectionReadError::Invalid);
        }
        Ok(Self {
            delivery_revision,
            publication_revision,
            candidate,
            result,
            source_seal,
        })
    }

    #[must_use]
    pub const fn delivery_revision(&self) -> u64 {
        self.delivery_revision
    }

    #[must_use]
    pub const fn publication_revision(&self) -> &Revision {
        &self.publication_revision
    }

    #[must_use]
    pub const fn candidate(&self) -> Option<&FrozenDeliveryCandidate> {
        self.candidate.as_ref()
    }

    #[must_use]
    pub const fn result(&self) -> Option<&PublicationResultFact> {
        self.result.as_ref()
    }

    pub(crate) const fn source_seal(&self) -> &Sha256Digest {
        &self.source_seal
    }
}

/// Trusted Git/publication intent and result reader.
pub trait TrustedPublicationProjectionAdapter: Send + Sync {
    /// Reads latest or exact publication facts for one aggregate revision.
    ///
    /// # Errors
    ///
    /// Returns a stable source failure without treating a missing adapter as an
    /// empty successful publication set.
    fn read_current(
        &self,
        scope: &RepositoryScope,
        delivery_id: &DeliveryId,
        delivery_revision: u64,
        expected_publication_revision: Option<&Revision>,
    ) -> Result<TrustedPublicationProjectionRead, TrustedProjectionReadError>;
}

/// Runtime and publication adapters installed as one immutable authority set.
pub struct StrongFlowProjectionSources {
    pub(crate) runtime: Box<dyn TrustedRuntimeProjectionAdapter>,
    pub(crate) publication: Box<dyn TrustedPublicationProjectionAdapter>,
}

impl StrongFlowProjectionSources {
    #[must_use]
    pub fn new(
        runtime: Box<dyn TrustedRuntimeProjectionAdapter>,
        publication: Box<dyn TrustedPublicationProjectionAdapter>,
    ) -> Self {
        Self {
            runtime,
            publication,
        }
    }
}

fn canonical_sha256(value: &Sha256Digest) -> bool {
    value
        .0
        .strip_prefix("sha256:")
        .is_some_and(lowercase_sha256)
}

fn lowercase_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

fn canonical_instant(value: &Instant) -> bool {
    let text = value.0.as_str();
    text.len() >= 20
        && text.len() <= 40
        && text.ends_with('Z')
        && text
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'-' | b':' | b'.' | b'T' | b'Z'))
}

fn portable(value: &str, max: usize) -> bool {
    !value.is_empty()
        && value.len() <= max
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(byte, b'.' | b'_' | b':' | b'/' | b'@' | b'-' | b'#')
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use winwincode_delivery::{
        domain::{Delivery, SessionBindingId},
        projection::runtime::{
            RuntimeProjection,
            test_support::{
                RuntimeAuthorityFixture, RuntimeFactFixture, accepted_binding, accepted_event,
            },
        },
    };

    fn runtime_projection() -> RuntimeProjection {
        let aggregate = Delivery::decode_json(include_bytes!(
            "../../../winwincode-delivery/tests/fixtures/delivery-main.json"
        ))
        .expect("canonical fixture");
        let binding_id = aggregate.snapshot().session_bindings[0].id.clone();
        let binding = accepted_binding(
            &aggregate,
            &SessionBindingId::new(binding_id.0).expect("binding id"),
            RuntimeAuthorityFixture::default(),
            Some(1),
        )
        .expect("accepted binding");
        let event = accepted_event(
            &binding,
            1,
            "runtime-checkpoint",
            RuntimeFactFixture::Checkpoint,
        )
        .expect("checkpoint");
        let mut projection = RuntimeProjection::new(&aggregate, vec![binding]).expect("projection");
        projection.apply(&event).expect("accepted checkpoint");
        projection
    }

    #[test]
    fn trusted_runtime_read_rejects_sequence_behind_fold() {
        let projection = runtime_projection();
        assert_eq!(projection.snapshot().sessions[0].as_of_sequence, 1);
        let read = TrustedRuntimeProjectionRead::try_new(
            7,
            Revision(1),
            1,
            Instant("2026-08-25T00:00:00Z".into()),
            projection.clone(),
            Sha256Digest(format!("sha256:{}", "a".repeat(64))),
        )
        .expect("bounded read");
        assert_eq!(read.snapshot(), projection.snapshot());

        assert_eq!(
            TrustedRuntimeProjectionRead::try_new(
                7,
                Revision(1),
                0,
                Instant("2026-08-25T00:00:00Z".into()),
                projection,
                Sha256Digest(format!("sha256:{}", "a".repeat(64))),
            )
            .expect_err("accepted cursor cannot trail its fold"),
            TrustedProjectionReadError::Invalid
        );
    }
}
