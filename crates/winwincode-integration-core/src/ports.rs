// SPDX-License-Identifier: Apache-2.0

use std::fmt;

use crate::{
    ConnectorAuthority, ConnectorRegistration, ConnectorRegistrationReceipt, InboundDispatch,
    InboundNormalizationContext, InboundReceipt, InboundWebhookRequest, IntegrationAuditFact,
    IntegrationError, IntegrationLeaseId, IntegrationOperationKey, IntegrationScope,
    NormalizedInboundEvent, OutboundAttemptResult, OutboundCallReceipt, OutboundClaim,
    OutboundDeliveryReceipt, OutboundEnqueueReceipt, OutboundOperation, OutboundRequest,
};
use winwincode_domain::IntegrationId;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SignatureVerificationErrorKind {
    Rejected,
    CredentialRevoked,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SignatureVerificationError {
    kind: SignatureVerificationErrorKind,
}

impl SignatureVerificationError {
    #[must_use]
    pub const fn rejected() -> Self {
        Self {
            kind: SignatureVerificationErrorKind::Rejected,
        }
    }

    #[must_use]
    pub const fn credential_revoked() -> Self {
        Self {
            kind: SignatureVerificationErrorKind::CredentialRevoked,
        }
    }

    #[must_use]
    pub const fn kind(self) -> SignatureVerificationErrorKind {
        self.kind
    }
}

impl fmt::Display for SignatureVerificationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("webhook signature was rejected")
    }
}

impl std::error::Error for SignatureVerificationError {}

pub trait WebhookSignatureVerifier {
    fn verify(
        &mut self,
        authority: &ConnectorAuthority,
        signature: &[u8],
        payload: &[u8],
    ) -> Result<(), SignatureVerificationError>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConnectorCallErrorKind {
    Retryable,
    Permanent,
    CredentialRevoked,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConnectorCallError {
    kind: ConnectorCallErrorKind,
    code: String,
    retry_after_millis: Option<u64>,
    network: Option<Box<winwincode_network::NetworkFailure>>,
    replay: winwincode_network::Replay,
}

impl ConnectorCallError {
    pub fn try_new(
        kind: ConnectorCallErrorKind,
        code: impl Into<String>,
    ) -> Result<Self, IntegrationError> {
        let code = code.into();
        let valid = !code.is_empty()
            && code.len() <= 64
            && code
                .bytes()
                .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_');
        if !valid {
            return Err(crate::model::invalid());
        }
        Ok(Self {
            kind,
            code,
            retry_after_millis: None,
            network: None,
            replay: winwincode_network::Replay::ReconcileFirst,
        })
    }

    #[must_use]
    pub fn from_network(code: &str, failure: winwincode_network::NetworkFailure) -> Self {
        use winwincode_network::ErrorKind;
        let kind = match failure.kind {
            ErrorKind::Authentication | ErrorKind::Authorization => {
                ConnectorCallErrorKind::CredentialRevoked
            }
            _ if failure.retryable() => ConnectorCallErrorKind::Retryable,
            _ => ConnectorCallErrorKind::Permanent,
        };
        let mut error = Self::try_new(kind, code).unwrap_or(Self {
            kind: ConnectorCallErrorKind::Permanent,
            code: "NETWORK_ERROR_CODE_INVALID".to_owned(),
            retry_after_millis: None,
            network: None,
            replay: winwincode_network::Replay::ReconcileFirst,
        });
        error.retry_after_millis = failure.retry_after_ms;
        error.network = Some(Box::new(failure));
        error
    }
    #[must_use]
    pub fn for_replay(mut self, replay: winwincode_network::Replay) -> Self {
        self.replay = replay;
        self
    }
    #[must_use]
    pub fn requires_reconciliation(&self) -> bool {
        self.replay == winwincode_network::Replay::ReconcileFirst
            && self.network.as_ref().is_some_and(|failure| {
                failure.acceptance != winwincode_network::Acceptance::NotSent
                    && !matches!(
                        failure.kind,
                        winwincode_network::ErrorKind::RateLimited
                            | winwincode_network::ErrorKind::Authentication
                            | winwincode_network::ErrorKind::Authorization
                            | winwincode_network::ErrorKind::RequestInvalid
                    )
                    && failure.http_status != Some(425)
            })
    }

    #[must_use]
    pub const fn network_failure(&self) -> Option<winwincode_network::NetworkFailure> {
        match &self.network {
            Some(failure) => Some(**failure),
            None => None,
        }
    }

    pub fn retryable_after(
        code: impl Into<String>,
        retry_after_millis: u64,
    ) -> Result<Self, IntegrationError> {
        if retry_after_millis == 0 || retry_after_millis > crate::model::MAX_SAFE_INTEGER {
            return Err(crate::model::invalid());
        }
        let mut error = Self::try_new(ConnectorCallErrorKind::Retryable, code)?;
        error.retry_after_millis = Some(retry_after_millis);
        Ok(error)
    }

    #[must_use]
    pub const fn kind(&self) -> ConnectorCallErrorKind {
        self.kind
    }

    #[must_use]
    pub fn code(&self) -> &str {
        &self.code
    }

    #[must_use]
    pub const fn retry_after_millis(&self) -> Option<u64> {
        self.retry_after_millis
    }
}

impl fmt::Display for ConnectorCallError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("connector call failed")
    }
}

impl std::error::Error for ConnectorCallError {}

pub trait ConnectorPort {
    fn normalize_inbound(
        &mut self,
        authority: &ConnectorAuthority,
        context: &InboundNormalizationContext,
        payload: &[u8],
    ) -> Result<NormalizedInboundEvent, ConnectorCallError>;

    fn deliver_outbound(
        &mut self,
        claim: &OutboundClaim,
    ) -> Result<OutboundCallReceipt, ConnectorCallError>;
}

#[allow(
    clippy::missing_errors_doc,
    reason = "every operation returns the stable IntegrationError boundary"
)]
pub trait IntegrationStoragePort {
    fn register(
        &mut self,
        registration: &ConnectorRegistration,
    ) -> Result<ConnectorRegistrationReceipt, IntegrationError>;

    fn authority(
        &self,
        scope: &IntegrationScope,
        integration_id: &IntegrationId,
    ) -> Result<ConnectorAuthority, IntegrationError>;

    fn revoke_credential(
        &mut self,
        scope: &IntegrationScope,
        integration_id: &IntegrationId,
        expected_revision: u64,
        occurred_at_millis: u64,
    ) -> Result<ConnectorAuthority, IntegrationError>;

    fn accept_inbound(
        &mut self,
        expected_authority: &ConnectorAuthority,
        request: &InboundWebhookRequest,
        normalized: &NormalizedInboundEvent,
    ) -> Result<InboundReceipt, IntegrationError>;

    fn inbound_dispatches(
        &self,
        scope: &IntegrationScope,
        integration_id: &IntegrationId,
        after_sequence: u64,
        limit: usize,
    ) -> Result<Vec<InboundDispatch>, IntegrationError>;

    fn enqueue_outbound(
        &mut self,
        request: &OutboundRequest,
    ) -> Result<OutboundEnqueueReceipt, IntegrationError>;

    fn claim_due(
        &mut self,
        scope: &IntegrationScope,
        integration_id: &IntegrationId,
        now_millis: u64,
        lease_id: IntegrationLeaseId,
        lease_expires_at_millis: u64,
    ) -> Result<Option<OutboundClaim>, IntegrationError>;

    fn record_success(
        &mut self,
        scope: &IntegrationScope,
        claim: &OutboundClaim,
        remote: &OutboundCallReceipt,
        completed_at_millis: u64,
    ) -> Result<OutboundDeliveryReceipt, IntegrationError>;

    fn record_failure(
        &mut self,
        scope: &IntegrationScope,
        claim: &OutboundClaim,
        failure: &ConnectorCallError,
        failed_at_millis: u64,
    ) -> Result<OutboundAttemptResult, IntegrationError>;

    fn outbound_operation(
        &self,
        scope: &IntegrationScope,
        integration_id: &IntegrationId,
        operation_key: &IntegrationOperationKey,
    ) -> Result<OutboundOperation, IntegrationError>;

    fn audit_facts(
        &self,
        scope: &IntegrationScope,
        integration_id: &IntegrationId,
        after_sequence: u64,
        limit: usize,
    ) -> Result<Vec<IntegrationAuditFact>, IntegrationError>;
}
