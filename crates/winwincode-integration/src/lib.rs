// SPDX-License-Identifier: Apache-2.0

//! Tenant-scoped enterprise connector and webhook delivery framework.

mod error;
mod framework;
mod github;
mod jira;
mod linear;
mod model;
mod ports;
mod slack;
mod storage;
mod teams;
mod webhook;

pub use error::{IntegrationError, IntegrationErrorKind};
pub use framework::IntegrationFramework;
pub use github::{
    GITHUB_CONNECTOR_PROTOCOL, GitHubAppId, GitHubClock, GitHubConnectorConfig,
    GitHubCredentialError, GitHubCredentialErrorKind, GitHubCredentialPort,
    GitHubEnterpriseConnector, GitHubEventMapperPort, GitHubInboundEvent, GitHubInstallationId,
    GitHubInstallationPermissions, GitHubInstallationToken, GitHubPermission, GitHubTlsRoots,
    GitHubWebhookHeaders, GitHubWebhookRequestFactory, GitHubWebhookSecret, GitHubWebhookVerifier,
};
pub use jira::{
    JIRA_CONNECTOR_PROTOCOL, JiraClock, JiraConnectorConfig, JiraCredentialError,
    JiraCredentialErrorKind, JiraCredentialPort, JiraEnterpriseConnector, JiraEventMapperPort,
    JiraInboundEvent, JiraOAuthAccessToken, JiraOAuthScope, JiraProjectKey, JiraResourceKind,
    JiraSiteId, JiraTlsRoots, JiraWebhookHeaders, JiraWebhookRequestFactory, JiraWebhookSecret,
    JiraWebhookVerifier,
};
pub use linear::{
    LINEAR_CONNECTOR_PROTOCOL, LinearClock, LinearCommentId, LinearConnectorConfig,
    LinearConnectorScope, LinearCredentialError, LinearCredentialErrorKind, LinearCredentialPort,
    LinearEnterpriseConnector, LinearEventAction, LinearEventKind, LinearEventMapperPort,
    LinearInboundEvent, LinearIssueId, LinearOAuthScope, LinearOAuthToken, LinearProjectId,
    LinearTeamId, LinearTlsRoots, LinearWebhookHeaders, LinearWebhookRequestFactory,
    LinearWebhookSecret, LinearWebhookVerifier, LinearWorkspaceId,
};
pub use model::{
    ConnectorAuthority, ConnectorProtocol, ConnectorRegistration, ConnectorRegistrationReceipt,
    ConnectorState, InboundDispatch, InboundNormalizationContext, InboundReceipt, InboundStatus,
    InboundWebhookMetadata, InboundWebhookRequest, IntegrationAuditFact, IntegrationAuditKind,
    IntegrationLeaseId, IntegrationOperationKey, NormalizedInboundEvent, OutboundAttemptResult,
    OutboundCallReceipt, OutboundClaim, OutboundDeliveryReceipt, OutboundEnqueueReceipt,
    OutboundOperation, OutboundOperationState, OutboundRequest, RetryPolicy,
};
pub use ports::{
    ConnectorCallError, ConnectorCallErrorKind, ConnectorPort, SignatureVerificationError,
    SignatureVerificationErrorKind, WebhookSignatureVerifier,
};
pub use slack::{
    SLACK_CONNECTOR_PROTOCOL, SlackAppId, SlackBotId, SlackBotPermissions, SlackBotToken,
    SlackChannelId, SlackClock, SlackConnectorConfig, SlackCredentialError,
    SlackCredentialErrorKind, SlackCredentialPort, SlackEnterpriseConnector,
    SlackInstallationIdentity, SlackInteractionAcknowledgement, SlackInteractionIngress,
    SlackRateLimitGate, SlackSigningProof, SlackSigningSecret, SlackTlsRoots, SlackWebApiMethod,
    SlackWebhookHeaders, SlackWebhookRequestFactory, SlackWebhookVerifier, SlackWorkspaceId,
    SystemSlackClock,
};
pub use storage::IntegrationStorage;
pub use teams::{
    MICROSOFT_TEAMS_CONNECTOR_PROTOCOL, TeamsChannelId, TeamsConnectorConfig, TeamsCredentialError,
    TeamsCredentialErrorKind, TeamsCredentialPort, TeamsEnterpriseConnector, TeamsGraphAccessToken,
    TeamsGraphCallError, TeamsGraphCallErrorKind, TeamsGraphClientState, TeamsGraphHttpTransport,
    TeamsGraphMessageReceipt, TeamsGraphOutboundMessage, TeamsGraphTlsRoots, TeamsGraphTokenClaims,
    TeamsGraphTokenValidationError, TeamsGraphTokenValidatorPort, TeamsGraphTransportPort,
    TeamsGraphValidationChallenge, TeamsGraphValidationResponse, TeamsGraphWebhookRequestFactory,
    TeamsGraphWebhookVerifier, TeamsTeamId, TeamsTenantId,
};
pub use webhook::{
    CredentialWebhookSignaturePort, GenericWebhookConnector, GenericWebhookVerifier,
    WEBHOOK_CONNECTOR_PROTOCOL, WebhookAddressResolverPort, WebhookAuthenticationMode,
    WebhookClock, WebhookConnectorConfig, WebhookCredentialError, WebhookCredentialErrorKind,
    WebhookCredentialPort, WebhookEndpoint, WebhookHmacSecret, WebhookHttpPort, WebhookHttpRequest,
    WebhookHttpResponse, WebhookInboundPolicy, WebhookInboundProof, WebhookLimits,
    WebhookMappingField, WebhookMappingTemplate, WebhookOutboundAuthentication,
    WebhookRequestFactory, WebhookSignaturePort,
};
pub use winwincode_domain::EnterpriseIntegrationId;
