// SPDX-License-Identifier: Apache-2.0

//! Storage- and product-neutral integration authority, receipts, and ports.

mod error;
mod framework;
#[doc(hidden)]
pub mod model;
mod ports;

pub use error::{IntegrationError, IntegrationErrorKind};
pub use framework::IntegrationFramework;
pub use model::{
    ConnectorAuthority, ConnectorProtocol, ConnectorRegistration, ConnectorRegistrationReceipt,
    ConnectorState, InboundDispatch, InboundNormalizationContext, InboundReceipt, InboundStatus,
    InboundWebhookMetadata, InboundWebhookRequest, IntegrationAuditFact, IntegrationAuditKind,
    IntegrationLeaseId, IntegrationOperationKey, IntegrationScope, NormalizedInboundEvent,
    OutboundAttemptResult, OutboundCallReceipt, OutboundClaim, OutboundDeliveryReceipt,
    OutboundEnqueueReceipt, OutboundOperation, OutboundOperationState, OutboundRequest,
    RetryPolicy,
};
pub use ports::{
    ConnectorCallError, ConnectorCallErrorKind, ConnectorPort, IntegrationStoragePort,
    SignatureVerificationError, SignatureVerificationErrorKind, WebhookSignatureVerifier,
};
pub use winwincode_domain::IntegrationId;
