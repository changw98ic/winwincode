// SPDX-License-Identifier: Apache-2.0

//! Application lifecycle host for the `WinWinCode` Control Plane.
//!
//! This crate owns application composition, Delivery persistence adapters, and
//! durable event publication. It has no dependency on Codex Core, an HTTP
//! server, or an Execution Worker runtime.

mod action_policy_enforcement;
mod artifact_enterprise_quota;
mod artifact_transaction;
mod candidate_git_release;
mod candidate_source;
mod chat_interaction_application;
pub mod chat_interaction_projection;
mod collaboration;
mod collaboration_inbox;
mod collaboration_inbox_production;
mod control_plane_instance;
pub mod credential_leak_gate;
pub mod credential_reference;
mod delivery_application;
mod delivery_command_transaction;
pub mod delivery_execution;
mod delivery_production_adapters;
mod delivery_transaction;
mod delivery_verdict_authority;
mod durable_execution_port;
mod enterprise_hierarchy;
mod enterprise_identity;
mod enterprise_identity_lifecycle;
mod enterprise_identity_protocols;
mod enterprise_identity_verification;
mod enterprise_policy;
mod enterprise_policy_enforcement;
mod enterprise_policy_evaluation;
mod enterprise_quota;
mod enterprise_rbac;
mod enterprise_reporting;
mod enterprise_scope_binding;
mod enterprise_usage;
pub mod execution_port_service;
mod gate_interaction_service;
pub mod local_secret_store;
mod model_admission;
mod model_execution_runtime;
mod model_policy_source;
mod model_request_pool;
mod model_retry_planner;
mod model_retry_settlement;
mod model_retry_usage;
mod model_route_availability;
pub mod model_settings;
mod model_stream_flow_control;
mod observer_decision_service;
mod planning_solution_authority;
mod product_session_execution_application;
mod product_session_service;
mod provider_admission;
mod provider_anthropic;
pub mod provider_catalog;
mod provider_enterprise_quota;
pub mod provider_gateway;
mod provider_https_sse;
mod provider_policy;
mod provider_production;
pub mod provider_stream;
mod publication_application;
mod publication_enterprise_quota;
mod publication_policy;
mod publication_policy_enforcement;
mod publication_preparation;
mod publication_production;
pub mod recovery_router;
mod remote_worker_pool;
mod repository_scheduler;
mod responsibility_assignment;
mod responsibility_assignment_authority;
mod rework_transaction;
mod runtime_event_transaction;
mod session_binding_transaction;
pub mod session_identity;
pub mod strongflow_projection;
mod task_breakdown_transaction;
mod temporary_root_lease;
mod terminal_outcome_transaction;
mod vault_kms_network;
mod vault_secret_store;
mod verdict_transaction;
mod worker_enterprise_quota;
mod worker_execution_lifecycle;
mod worker_fleet_projection;
mod worker_interaction_outbound;
pub mod worker_management;
mod worker_policy;

pub use action_policy_enforcement::{
    ActionPolicyEnforcementError, ActionPolicyEnforcementErrorKind,
    issue_action_enforcement_receipt,
};
pub use artifact_enterprise_quota::{
    ArtifactEnterpriseQuotaAdmission, ArtifactEnterpriseQuotaReservation,
    ArtifactEnterpriseQuotaSaga, ArtifactEnterpriseQuotaSagaError, ArtifactEnterpriseUsagePort,
    DurableArtifactEnterpriseUsage,
};
pub use artifact_transaction::ArtifactMessageError;
pub use candidate_git_release::CandidateGitReadsClosedReceipt;
pub use candidate_source::CandidateResolutionError;
pub use chat_interaction_application::{
    ApprovalDecideMutationReceipt, ChatInteractionApiService, ChatInteractionMutationReceipt,
    ChatInteractionService, ChatInteractionServiceError, ChatInteractionServiceErrorCode,
    ChatInteractionWriteStatus, InputRespondMutationReceipt, RecordApprovalInteractionCommand,
    RecordInputInteractionCommand, WorkerInteractionAuthority, WorkerInteractionDeliveryError,
    WorkerInteractionDeliveryErrorKind, WorkerInteractionOutboundPort,
};
pub use collaboration::{
    CollaborationActivityRecordRequest, CollaborationClock, CollaborationClockError,
    CollaborationError, CollaborationErrorKind, CollaborationService, SystemCollaborationClock,
};
pub use collaboration_inbox::{
    CollaborationAnnotation, CollaborationAnnotationAction, CollaborationAnnotationCommand,
    CollaborationAnnotationId, CollaborationAnnotationState, CollaborationAnnotationTarget,
    CollaborationCandidateIdentity, CollaborationClaim, CollaborationClaimAction,
    CollaborationClaimCommand, CollaborationInboxAudience, CollaborationInboxAuthorityError,
    CollaborationInboxAuthorityPort, CollaborationInboxAuthoritySnapshot, CollaborationInboxClock,
    CollaborationInboxClockError, CollaborationInboxCommandContext, CollaborationInboxError,
    CollaborationInboxErrorKind, CollaborationInboxFilter, CollaborationInboxItem,
    CollaborationInboxItemId, CollaborationInboxItemKind, CollaborationInboxItemState,
    CollaborationInboxListRequest, CollaborationInboxPage, CollaborationInboxReceipt,
    CollaborationInboxService, CollaborationInboxSourceError, CollaborationInboxSourceItem,
    CollaborationInboxSourcePort, CollaborationInboxSourceSnapshot,
    CollaborationResponsibilityEntitlement, FormalCollaborationCommandRoute,
    SystemCollaborationInboxClock,
};
pub use collaboration_inbox_production::{
    DurableCollaborationInboxSource, EnterpriseCollaborationInboxAuthority,
};
pub use control_plane_instance::{
    ControlPlaneInstanceRuntime, ControlPlaneInstanceRuntimeConfig,
    ControlPlaneInstanceRuntimeError, ControlPlaneInstanceRuntimeErrorKind,
};
pub use credential_leak_gate::{
    CredentialLeakError, CredentialLeakErrorKind, CredentialLeakGate, CredentialOutputBoundary,
};
pub use credential_reference::{
    CredentialReferenceError, CredentialReferenceErrorKind, CredentialReferenceResolution,
    CredentialReferenceService, CredentialSecretResolutionError, ResolvedSecret, SecretStoreError,
    SecretStoreErrorKind, SecretStorePort,
};
pub use delivery_application::{
    DeliveryAdvanceAuthority, DeliveryApplicationError, DeliveryAttentionAuthority,
    DeliveryAuthorityError, DeliveryAuthorityPort, DeliveryAuthorityRequest, DeliveryAuthoritySeal,
    DeliverySpecificationAuthority, DeliveryVerdictAuthority, load_delivery_authority_seal,
};
pub use delivery_command_transaction::{DeliveryCommandFacts, DeliverySpecFacts};
pub use delivery_production_adapters::{
    LocalDeliveryAdapterConfig, LocalDeliveryAdapterError, LocalDeliveryAuthority,
    LocalExecutionJobDispatcher,
};
pub use durable_execution_port::{
    DurableExecutionPortContext, DurableExecutionPortDelegate, DurableExecutionPortError,
    DurableExecutionPortIngress, DurableExecutionPortSupplement,
};
pub use enterprise_hierarchy::{
    EnterpriseHierarchyCommand, EnterpriseHierarchyError, EnterpriseHierarchyErrorKind,
    EnterpriseHierarchyReceipt, EnterpriseHierarchyService, EnvironmentId, HierarchyMutation,
    HierarchyResource, HierarchyResourceId, HierarchyResourceKind, HierarchyResourceState,
    HierarchyScope, ResolvedHierarchyResource,
};
pub use enterprise_identity::{
    AuthenticatedEnterpriseIdentity, EnterpriseIdentityClock, EnterpriseIdentityClockError,
    EnterpriseIdentityError, EnterpriseIdentityErrorKind, EnterpriseIdentityService,
    GeneratedApiToken, SystemEnterpriseIdentityClock, generate_api_token,
};
pub use enterprise_identity_lifecycle::{
    BrowserSessionLifecycleError, BrowserSessionLifecyclePort,
    CanonicalEnterpriseIdentityLifecycle, DeprovisionExternalUser,
    EnterpriseIdentityLifecycleError, EnterpriseIdentityLifecycleErrorKind,
    EnterpriseIdentityLifecyclePort, ExternalIdentityLifecycleOutcome, ExternalIdentityPrincipal,
    ExternalIdentityProvider, ExternalIdentityReference, ProvisionExternalUser, UpsertExternalTeam,
    external_identity_id, membership_id,
};
pub use enterprise_identity_protocols::{
    EnterpriseIdentityProtocolAdapter, EnterpriseIdentityProtocolConfig, EnterpriseProtocolClock,
    EnterpriseProtocolClockError, EnterpriseProtocolError, EnterpriseProtocolErrorKind,
    ExternalAuthenticationOutcome, OidcIdToken, OidcTokenVerifier, ProtocolVerificationError,
    ProtocolVerificationErrorKind, SamlResponse, SamlResponseVerifier, ScimBearerToken,
    ScimBearerVerifier, ScimLifecycleEvent, ScimOperation, ScimTeamUpsert, ScimUserDeprovision,
    ScimUserProvision, SystemEnterpriseProtocolClock, TrustedProtocolParty, VerifiedOidcClaims,
    VerifiedSamlClaims, VerifiedScimClient,
};
pub use enterprise_identity_verification::{
    EnterpriseIdentityProductionVerifiers, EnterpriseIdentityVerifierConfig,
    EnterpriseIdentityVerifierTimeouts, EnterpriseIdentityVerifierTlsRoots,
    ProductionOidcTokenVerifier, ProductionSamlResponseVerifier, ProductionScimBearerVerifier,
};
pub use enterprise_policy::{
    EnterprisePolicyApiError, EnterprisePolicyApiErrorKind, EnterprisePolicyApiService,
    EnterprisePolicyClock,
};
pub use enterprise_policy_enforcement::{
    EnterprisePolicyEnforcement, EnterprisePolicyEnforcementError,
    EnterprisePolicyEnforcementErrorKind, EnterprisePolicyEnforcementRequest,
    enforce_enterprise_policy, enterprise_policy_condition_sha256,
    enterprise_policy_subject_sha256,
};
pub use enterprise_policy_evaluation::{
    EnterprisePolicyDecisionClock, EnterprisePolicyEvaluationService,
    EnterprisePolicyEvaluationTarget, EnterprisePolicyExceptionDecisionRequest,
    EnterprisePolicyExceptionOpenRequest,
};
pub use enterprise_quota::{
    DurableEnterpriseQuotaAdmission, EnterpriseQuotaAdmission, EnterpriseQuotaAdmissionPort,
    EnterpriseQuotaPermit,
};
pub use enterprise_rbac::{
    ActiveMemberContext, ActiveTeamContext, EnterpriseRbacClock, EnterpriseRbacClockError,
    EnterpriseRbacError, EnterpriseRbacErrorKind, EnterpriseRbacService, EvaluatedRoleVersion,
    RbacAuthoritySeal, RbacDecision, RbacDenialReason, SystemEnterpriseRbacClock,
};
pub use enterprise_reporting::{
    EnterpriseReportCurrencyRule, EnterpriseReportCursor, EnterpriseReportDetail,
    EnterpriseReportDimension, EnterpriseReportError, EnterpriseReportErrorKind,
    EnterpriseReportExport, EnterpriseReportFormat, EnterpriseReportGroup, EnterpriseReportPage,
    EnterpriseReportQuery, EnterpriseReportRow, EnterpriseReportTimeRule, EnterpriseReportTotals,
    EnterpriseReportingLimits, EnterpriseReportingProjection, EnterpriseReportingService,
};
pub use enterprise_scope_binding::{
    EnterpriseScopeBinding, EnterpriseScopeBindingCommand, EnterpriseScopeBindingError,
    EnterpriseScopeBindingErrorKind, EnterpriseScopeBindingMutation, EnterpriseScopeBindingReceipt,
    EnterpriseScopeBindingService, LocalScopeMigrationCommand, LocalScopeMigrationReceipt,
    LocalScopeMigrationStatus, ResolvedScopeBinding, ScopeBindingSource, ScopeBindingSubject,
    ScopeBindingSubjectKind, local_scope_inventory_digest,
};
pub use enterprise_usage::{
    ProviderEnterpriseUsageError, ProviderEnterpriseUsageErrorKind,
    ProviderEnterpriseUsageReconciler, ProviderEnterpriseUsageReconciliation,
    PublicationEnterpriseUsageError, PublicationEnterpriseUsageErrorKind,
    PublicationEnterpriseUsageReconciler, PublicationEnterpriseUsageReconciliation,
    StorageEnterpriseUsageError, StorageEnterpriseUsageErrorKind, StorageEnterpriseUsageReconciler,
    StorageEnterpriseUsageReconciliation, WorkerEnterpriseUsageError,
    WorkerEnterpriseUsageErrorKind, WorkerEnterpriseUsageReconciler,
    WorkerEnterpriseUsageReconciliation,
};
pub use execution_port_service::{
    DEFAULT_HEARTBEAT_INTERVAL_MS, ExecutionPortService, ExecutionPortServiceError,
    RuntimeReplayRequestCommand,
};
pub use gate_interaction_service::{
    ExpireGateInteractionCommand, GATE_INTERACTION_SCHEMA_VERSION, GateActionSeal,
    GateCandidateIdentity, GateDecisionFact, GateHumanDecision, GateInteractionActor,
    GateInteractionAuthority, GateInteractionCommandContext, GateInteractionMutationReceipt,
    GateInteractionRecord, GateInteractionService, GateInteractionServiceError,
    GateInteractionServiceErrorCode, GateInteractionState, GateInteractionSubject,
    RegisterGateInteractionCommand, RespondGateInteractionCommand, RoutableGateDecision,
    RoutableGateDecisionKind,
};
pub use local_secret_store::{
    LocalSecretCleanupReceipt, LocalSecretStoreAdapter, LocalSecretWriteReceipt,
};
pub use model_admission::{
    FrozenModelAdmissionPolicy, FrozenModelRouteAuthority, ModelAdmissionClock,
    ModelAdmissionClockError, ModelAdmissionDenialReason, ModelAdmissionError,
    ModelAdmissionErrorKind, ModelAdmissionLimits, ModelAdmissionPolicyLayer,
    ModelAdmissionService, ModelAdmissionSnapshot, ModelPolicySource, ModelReservationCompletion,
    ModelReservationReceipt, ModelReservationRelease, ModelReservationReleaseReason,
    ModelReservationRequest, ModelReservationTerminalOutcome, ModelReservationTerminalReceipt,
    ModelRoutePolicyDecision,
};
pub use model_execution_runtime::{
    DurableModelExchangeAuthority, ModelExecutionAckReceipt, ModelExecutionBatchReceipt,
    ModelExecutionOpenReceipt, ModelExecutionPortReceipt, ModelExecutionRuntime,
    ModelExecutionRuntimeError, ModelExecutionRuntimeErrorKind,
};
pub use model_policy_source::{
    EnterpriseModelPolicyCeiling, LocalModelPolicyAuthority, LocalModelPolicyAuthorityConfig,
    ModelPolicyAuthorityError, ModelPolicyAuthorityPort, ModelPolicyAuthoritySnapshot,
    ModelPolicyResolution, ModelPolicyResolutionError, ModelPolicyResolutionErrorKind,
    ModelPolicyRouteKey, ProductionModelPolicySource,
};
pub use model_request_pool::{
    ModelFrameAckReceipt, ModelFrameWriteReceipt, ModelFrameWriteStatus, ModelRequestAdmission,
    ModelRequestAdmissionReceipt, ModelRequestAdmissionStatus, ModelRequestPool,
    ModelRequestPoolConfig, ModelRequestPoolError, ModelRequestPoolErrorCode, ModelRequestRouteKey,
    ModelRequestSnapshot, ModelRequestState, ModelRequestTerminalOutcome,
    ModelRequestTerminalReceipt, ModelRoutePoolSnapshot, ModelStreamFrame, ModelStreamReadControl,
};
pub use model_retry_planner::{
    ConfiguredModelRetryPlanAuthority, DurableModelRetryPreOpenPlanner,
    ModelRetryAdmissionReleaseAuthority, ModelRetryPlanAuthorityPort, ModelRetryPlannerError,
    ModelRetryPlannerErrorKind, ModelRetryPreOpenPlannerPort,
};
pub use model_retry_settlement::{
    DurableModelRetryContextSource, DurableProviderRetrySettlement, ModelRetrySettlementContext,
    ModelRetrySettlementContextError, ModelRetrySettlementContextErrorKind,
    ModelRetrySettlementContextPort, ModelRetrySettlementError, ModelRetrySettlementErrorKind,
    ModelRetrySettlementReceipt,
};
pub use model_retry_usage::{
    FrozenModelRetryPlan, ModelAttemptCharge, ModelAttemptCompletionCommand,
    ModelAttemptFailureCommand, ModelAttemptFailureFact, ModelAttemptFailureKind,
    ModelAttemptStartCommand, ModelAttemptStartReceipt, ModelExecutionCertainty, ModelRetryAction,
    ModelRetryDecisionReceipt, ModelRetryStep, ModelRetryUsageError, ModelRetryUsageErrorKind,
    ModelRetryUsageRequest, ModelRetryUsageService, ModelUsageAttribution, ModelUsageFilter,
    ModelUsageReconciliation, ModelUsageSettlementReceipt, ModelUsageSourceCursor,
    ModelUsageSourceEntry, ModelUsageSourcePage, ModelUsageTotals, SettledModelUsage,
};
pub use model_route_availability::{
    ModelRouteAvailabilityError, ModelRouteAvailabilityErrorKind, ModelRouteAvailabilityService,
};
pub use model_settings::{
    DEFAULT_WORKER_CONCURRENCY_LIMIT, ModelSelection, ModelSettingsChange, ModelSettingsError,
    ModelSettingsErrorKind, ModelSettingsMutationReceipt, ModelSettingsProjection,
    ModelSettingsRequest, ModelSettingsService, ModelSettingsTarget, ModelSettingsValues,
};
pub use model_stream_flow_control::{
    ModelStreamFlowAckReceipt, ModelStreamFlowCancellationReceipt, ModelStreamFlowCoordinator,
    ModelStreamFlowError, ModelStreamFlowErrorKind, ModelStreamFlowWriteReceipt,
};
pub use observer_decision_service::{
    ApplyObserverCheckpointCommand, OBSERVER_DECISION_SERVICE_SCHEMA_VERSION,
    ObserverCheckpointKind, ObserverDecisionCommandContext, ObserverDecisionInput,
    ObserverDecisionKind, ObserverDecisionMutationReceipt, ObserverDecisionOrigin,
    ObserverDecisionPersistence, ObserverDecisionProjection, ObserverDecisionService,
    ObserverDecisionServiceError, ObserverDecisionServiceErrorCode, ObserverDecisionState,
    ObserverExecutionSource, ObserverRetainedResources, ObserverRuntimeTraceRef,
    ObserverSafeCheckpoint, RecordObserverDecisionCommand,
};
pub use product_session_execution_application::{
    ProductSessionExecutionApplication, ProductSessionExecutionApplicationError,
};
pub(crate) use product_session_service::ReplaceProductSessionExecutionBindingCommand;
pub use product_session_service::{
    AppendAssistantMessageCommand, AssistantMessageMutationReceipt, AssistantMessageState,
    CancelProductSessionCommand, ChatSubmitMutationReceipt, CloseProductSessionCommand,
    ContinueProductSessionCommand, CreateProductSessionCommand, DurableSessionBinding,
    ForkProductSessionCommand, PRODUCT_SESSION_SERVICE_SCHEMA_VERSION, ProductSessionApiClock,
    ProductSessionApiService, ProductSessionAuthoritySeal, ProductSessionCancellationReceipt,
    ProductSessionCommandContext, ProductSessionExecutionConfig, ProductSessionMessagePage,
    ProductSessionMutationReceipt, ProductSessionPageRead, ProductSessionPageRequest,
    ProductSessionPersistence, ProductSessionRecord, ProductSessionService,
    ProductSessionServiceError, ProductSessionServiceErrorCode, ProductSessionTurnIntent,
    ProductSessionTurnState, ProductSessionTurnTerminalOutcome, RecordAssistantTerminalCommand,
    SubmitChatMessageCommand, product_session_command_context, product_session_state_filters,
};
pub use provider_admission::{
    DurableProviderGatewayAdmission, ProviderAdmissionError, ProviderAdmissionErrorKind,
    ProviderAdmissionOpenReceipt, ProviderAdmissionOpenRequest, ProviderAdmissionReservationConfig,
    ProviderGatewayAdmissionPort, SystemModelAdmissionClock,
};
pub use provider_anthropic::ProviderTokenPricing;
pub use provider_catalog::{
    CatalogAvailability, ModelCapability, ModelCapabilityProjection, ModelCatalogVersion,
    ModelToolSupport, PROVIDER_CATALOG_VERSION_EVENT_TOPIC, ProviderCatalogChange,
    ProviderCatalogEntryProjection, ProviderCatalogError, ProviderCatalogErrorKind,
    ProviderCatalogMutationReceipt, ProviderCatalogProjection, ProviderCatalogRequest,
    ProviderCatalogService, ProviderCatalogVersionEvent, ProviderDescriptor,
    ResolvedModelCapability,
};
pub use provider_enterprise_quota::{
    DurableProviderEnterpriseUsageSource, ProviderEnterpriseQuotaError,
    ProviderEnterpriseQuotaErrorKind, ProviderEnterpriseQuotaOpen,
    ProviderEnterpriseQuotaReservation, ProviderEnterpriseQuotaSaga,
    ProviderEnterpriseUsageSourcePort, ProviderOperationalAdmissionError,
    ProviderOperationalAdmissionPort,
};
pub use provider_gateway::{
    ProviderAdapterError, ProviderAdapterErrorKind, ProviderAdapterInvocation,
    ProviderAdapterOpenReceipt, ProviderAdapterPort, ProviderGateway,
    ProviderGatewayDurableExchange, ProviderGatewayError, ProviderGatewayErrorKind,
    ProviderGatewayIdentity, ProviderGatewayIdentityError, ProviderGatewayIdentityErrorKind,
    ProviderGatewayIdentityPort, ProviderGatewayOpenReceipt, ProviderGatewaySettlement,
    ProviderGatewaySettlementError, ProviderGatewaySettlementPort, ProviderGatewayTerminal,
    ProviderGatewayTerminalCharge, ProviderGatewayTerminalOutcome, ProviderGatewayTerminalProgress,
    ProviderGatewayTerminalProgressPort, ProviderGatewayTerminalProgressStage,
    ProviderGatewayTerminalReceipt, ProviderStreamControlAction, ProviderStreamControlReceipt,
};
pub use provider_https_sse::{
    HttpsSseProviderAdapter, HttpsSseProviderCompletion, HttpsSseProviderConfig,
    HttpsSseProviderError, HttpsSseProviderErrorKind, HttpsSseProviderLimits,
    HttpsSseProviderTimeouts, ProviderTlsRoots,
};
pub use provider_policy::{
    DurableProviderPolicyEnforcement, ProviderPolicyError, ProviderPolicyErrorKind,
    ProviderPolicyReceipt,
};
pub use provider_production::{
    DeterministicLoopbackProviderAdapter, DurableProviderGatewayIdentitySource,
    StandaloneModelExecutionApplication, StandaloneModelExecutionConfig,
    StandaloneModelExecutionError, StandaloneModelExecutionErrorKind, StandaloneProviderConfig,
    local_loopback_retry_policy,
};
pub use provider_stream::{
    CanonicalModelStreamFrame, ProviderFinishReason, ProviderStreamConversionError,
    ProviderStreamConversionErrorKind, ProviderStreamConverter, ProviderStreamEvent,
    ProviderStreamFailure, ProviderStreamFailureKind, ProviderTokenUsage, ProviderToolIdentity,
    ProviderToolIdentityError, ProviderToolKind,
};
pub use publication_enterprise_quota::PublicationEnterpriseQuotaSaga;
pub use publication_policy::PublicationCommandError;
pub use publication_policy_enforcement::{
    DurablePublicationPolicyEnforcement, PublicationEnterprisePolicyError,
    PublicationEnterprisePolicyErrorKind,
};
pub use publication_preparation::{PreparedPublication, PublicationPreparationError};
pub use publication_production::{
    LocalGitHubProviderConfig, LocalPublicationAdapterConfig, LocalPublicationAdapterError,
    LocalPublicationAuthority, LocalPublicationAuthorityConfig, LocalPublicationProviderRegistry,
    PublicationAuthorityError, PublicationAuthorityErrorKind, PublicationAuthorityFacts,
    PublicationAuthorityPort, PublicationAuthorityRequest, PublicationProviderRegistry,
    PublicationProviderRegistryError, PublicationProviderRegistryErrorKind,
    PublicationProviderSession,
};
pub use remote_worker_pool::{
    RemoteWorkerAuthenticationError, RemoteWorkerAuthenticationErrorKind,
    RemoteWorkerAuthenticator, RemoteWorkerConnection, RemoteWorkerConnectionState,
    RemoteWorkerCredential, RemoteWorkerPoolAdapter, RemoteWorkerPoolError,
    RemoteWorkerPoolErrorKind, RemoteWorkerPrincipal,
};
pub use repository_scheduler::{RepositoryExecutionScheduler, RepositoryExecutionSchedulerError};
pub use responsibility_assignment::{
    ResponsibilityAssignment, ResponsibilityAssignmentAction, ResponsibilityAssignmentClock,
    ResponsibilityAssignmentClockError, ResponsibilityAssignmentCommand,
    ResponsibilityAssignmentContext, ResponsibilityAssignmentError,
    ResponsibilityAssignmentErrorKind, ResponsibilityAssignmentId,
    ResponsibilityAssignmentListRequest, ResponsibilityAssignmentOperation,
    ResponsibilityAssignmentReceipt, ResponsibilityAssignmentService,
    ResponsibilityAssignmentState, ResponsibilityAuthorityError, ResponsibilityAuthorityPort,
    ResponsibilityAuthorityRequest, ResponsibilityCommandAuthority, ResponsibilityInboxAuthority,
    ResponsibilityInboxSnapshot, ResponsibilityListAuthority, ResponsibilityPrincipalAuthority,
    ResponsibilityReviewKind, ResponsibilityRole, ResponsibilityTarget,
    SystemResponsibilityAssignmentClock,
};
pub use responsibility_assignment_authority::{
    DurableResponsibilityTargetAuthority, EnterpriseResponsibilityAuthority,
    ResponsibilityTargetAuthorityPort, ResponsibilityTargetAuthoritySeal,
};
pub use runtime_event_transaction::RuntimeMessageError;
pub use session_identity::{
    SessionBindingAcceptance, SessionIdentityAdapterError, validate_session_binding,
};
pub use temporary_root_lease::{
    OwnedTemporaryRoot, SystemTemporaryRootLeaseRuntime, TEMPORARY_ROOT_LEASE_FILE,
    TemporaryRootLease, TemporaryRootLeaseConfig, TemporaryRootLeaseError,
    TemporaryRootLeaseErrorKind, TemporaryRootLeaseManager, TemporaryRootLeaseRuntime,
    TemporaryRootReclaimReport, TemporaryRootTarget,
};
pub use vault_kms_network::{
    VaultKmsNetworkAdapter, VaultKmsNetworkConfig, VaultKmsNetworkKeyRotationReceipt,
    VaultKmsNetworkLeaseReceipt, VaultKmsNetworkLeasedSecret, VaultKmsNetworkRevocationReceipt,
    VaultKmsNetworkTimeouts, VaultKmsNetworkTlsRoots, VaultKmsNetworkWriteReceipt,
    VaultKmsWorkloadCredential, VaultKmsWorkloadIdentityPort,
};
pub use vault_secret_store::{
    SystemVaultKmsClock, VaultKmsClock, VaultKmsClockError, VaultKmsKeyMaterial, VaultKmsKeyring,
    VaultKmsRewrapReceipt, VaultKmsSecretStoreAdapter, VaultLeasedSecret,
    VaultSecretCleanupReceipt, VaultSecretLeaseReceipt, VaultSecretWriteReceipt,
};
pub use worker_enterprise_quota::{
    WorkerEnterpriseQuotaAuthority, WorkerEnterpriseQuotaAuthorityPort, WorkerEnterpriseQuotaClaim,
    WorkerEnterpriseQuotaError, WorkerEnterpriseQuotaErrorKind, WorkerEnterpriseQuotaReservation,
    WorkerEnterpriseQuotaSaga, WorkerEnterpriseUsageSourcePort, WorkerOperationalClaimPort,
};
pub use worker_execution_lifecycle::{
    DurableWorkerExecutionLifecycle, WorkerExecutionLifecycleError,
    WorkerExecutionLifecycleErrorKind, WorkerExecutionRelease, WorkerExecutionTerminalReceipt,
    WorkerExecutionUsageSettlement,
};
pub use worker_fleet_projection::{
    WorkerFleetProjectionService, WorkerFleetProjectionServiceError,
    WorkerFleetProjectionServiceErrorKind,
};
pub use worker_interaction_outbound::{
    DurableWorkerInteractionOutbound, WorkerInteractionClaim, WorkerInteractionClaimPage,
    WorkerInteractionConnectionError, WorkerInteractionConnectionErrorKind,
    WorkerInteractionPageCursor,
};
pub use worker_policy::{DurableWorkerPolicyEnforcement, WorkerPolicyError, WorkerPolicyErrorKind};

use std::fmt;
use std::path::{Path, PathBuf};

use delivery_execution::{
    DeliveryExecutionDispatchReceipt, DeliveryExecutionError, DeliveryExecutionPortError,
    ExecutionJobDispatcher, PendingDeliveryExecution,
};
pub use session_binding_transaction::{
    DeliverySessionBindingCommitError, DeliverySessionBindingCommitReceipt,
};
use sha2::{Digest, Sha256};
pub use terminal_outcome_transaction::{
    DeliveryTerminalOutcomeCommitError, DeliveryTerminalOutcomeCommitReceipt,
};
use winwincode_api::generated::{
    Actor, CommandEnvelope, CommandName, ControlPlaneWebSocketDeliveryChangedEvent,
    ControlPlaneWebSocketDeliveryChangedEventTypeValue, ControlPlaneWebSocketEventType,
    RepositoryScope, Scope,
};
use winwincode_audit::{
    AuditAction, AuditActor, AuditEvent, AuditEventId, AuditOrigin, AuditRetention, AuditScope,
    AuditState, AuditStore, AuditSubject,
};
use winwincode_delivery::application::{CoordinationError, verdict::SubmitVerdictFacts};
use winwincode_delivery::domain::Delivery;
use winwincode_domain::{
    ControlPlaneEventId, DeliveryId, Instant, RequestId, Revision, Sha256Digest, SystemActorId,
};
use winwincode_execution_port::generated::{self as execution_port, ExecutionScope};
pub use winwincode_storage::{
    AggregateJournalKey, AggregateJournalPublication, AggregateJournalRecord, ArtifactObjectStore,
    ArtifactStore, CandidateGitPinReceipt, CandidateGitReleaseAuthority,
    CandidateGitReleaseReceipt, CandidateGitRetentionError, CandidateGitTerminalOutcome,
    CommitReceipt, DurableOutboxEvent, GitSourceResolver, LoadedAggregateJournal, NewOutboxEvent,
    OutboxEvent, PendingAuditEvent, ProductStateStorage, ProjectionEventCursor,
    ProjectionEventStream, ProjectionEventStreamKey, StorageError, StorageErrorKind, StoredState,
};
use winwincode_storage::{
    LocalArtifactObjectStore, PublicEventActor, PublicEventScope, PublicEventSource,
    ReceiptActorKey, ReceiptIdentity, ReceiptScopeKey, SqliteStorage, StateCommit,
    StateRevisionGuard, public_receipt_identity as storage_public_receipt_identity,
    receipt_actor_key as storage_receipt_actor_key, receipt_scope_key as storage_receipt_scope_key,
    repository_scope_from_receipt_key as storage_repository_scope_from_receipt_key,
};
pub use worker_management::{
    ScopeWorkerHealthEventPort, WORKER_HEALTH_CHANGED_TOPIC, WorkerHealthEventPort,
    WorkerHealthEventPortError, WorkerHealthEventPortErrorKind, WorkerHealthEventRequest,
    WorkerManagementService, WorkerManagementServiceError, WorkerManagementServiceErrorKind,
};

/// Deterministic trusted-fact fixtures for exercising the typed Control Plane
/// seam without exposing production authority constructors.
#[cfg(feature = "test-support")]
pub mod test_support {
    use winwincode_api::generated::{CommandEnvelope, RepositoryScope};
    use winwincode_delivery::{
        application::{attention::ResolvedAttentionTransition, stage::StageAdvanceResult},
        domain::{DeliverySourceRef, RepositoryRef},
    };

    use super::{
        DeliveryCommandFacts, DeliverySpecFacts, StorageError,
        delivery_command_transaction::TrustedDeliverySpecFacts,
    };

    /// Complete product-owned Spec semantics used by integration fixtures.
    #[derive(Clone, Debug, PartialEq)]
    pub struct DeliverySpecFactsFixture {
        pub repository_scope: RepositoryScope,
        pub now_millis: u64,
        pub repository: RepositoryRef,
        pub source_ref: Option<DeliverySourceRef>,
        pub scope: Vec<String>,
        pub out_of_scope: Vec<String>,
        pub constraints: Vec<String>,
        pub max_rework_attempts: u64,
        pub criterion_verification_methods: Vec<(String, String)>,
    }

    /// Adapter-confirmed repository authority used by sealed stage fixtures.
    #[derive(Clone, Debug, PartialEq)]
    pub struct DeliveryRepositoryFactsFixture {
        pub repository_scope: RepositoryScope,
        pub repository: RepositoryRef,
        pub source_ref: Option<DeliverySourceRef>,
    }

    /// Binds trusted repository, time, and exact criterion verification facts
    /// to one create or Spec-replacement command.
    ///
    /// # Errors
    ///
    /// Returns an error when the command and adapter-confirmed repository
    /// scope do not identify the same canonical authority.
    pub fn delivery_spec_command_facts(
        command: &CommandEnvelope,
        fixture: DeliverySpecFactsFixture,
    ) -> Result<DeliveryCommandFacts, StorageError> {
        let repository_scope = fixture.repository_scope.clone();
        DeliveryCommandFacts::specification_from_trusted_adapter(
            command,
            repository_scope,
            DeliverySpecFacts::from_trusted_adapter(TrustedDeliverySpecFacts {
                now_millis: fixture.now_millis,
                repository: fixture.repository,
                source_ref: fixture.source_ref,
                scope: fixture.scope,
                out_of_scope: fixture.out_of_scope,
                constraints: fixture.constraints,
                max_rework_attempts: fixture.max_rework_attempts,
                criterion_verification_methods: fixture.criterion_verification_methods,
            }),
        )
    }

    /// Binds one production-sealed human stage transition to its exact command.
    ///
    /// # Errors
    ///
    /// Returns an error when the command and adapter-confirmed repository
    /// scope do not identify the same canonical authority.
    pub fn delivery_advance_command_facts(
        command: &CommandEnvelope,
        repository: DeliveryRepositoryFactsFixture,
        transition: StageAdvanceResult,
    ) -> Result<DeliveryCommandFacts, StorageError> {
        DeliveryCommandFacts::advance_from_trusted_adapter(
            command,
            repository.repository_scope,
            repository.repository,
            repository.source_ref,
            transition,
        )
    }

    /// Binds one production-sealed Attention transition to its exact command.
    ///
    /// # Errors
    ///
    /// Returns an error when the command and adapter-confirmed repository
    /// scope do not identify the same canonical authority.
    pub fn delivery_attention_command_facts(
        command: &CommandEnvelope,
        repository: DeliveryRepositoryFactsFixture,
        transition: ResolvedAttentionTransition,
    ) -> Result<DeliveryCommandFacts, StorageError> {
        DeliveryCommandFacts::attention_from_trusted_adapter(
            command,
            repository.repository_scope,
            repository.repository,
            repository.source_ref,
            transition,
        )
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DeliveryChangeKind {
    Created,
    Advanced,
    Reworked,
}

impl DeliveryChangeKind {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Advanced => "advanced",
            Self::Reworked => "reworked",
        }
    }
}

/// Canonical state and outbox values produced by one validated application command.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StateChange {
    pub stream_id: String,
    pub state: Vec<u8>,
    pub events: Vec<NewOutboxEvent>,
}

impl StateChange {
    #[must_use]
    pub fn new(
        stream_id: impl Into<String>,
        state: impl Into<Vec<u8>>,
        events: Vec<NewOutboxEvent>,
    ) -> Self {
        Self {
            stream_id: stream_id.into(),
            state: state.into(),
            events,
        }
    }
}

/// Local Control Plane process configuration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ControlPlaneConfig {
    data_directory: PathBuf,
    temporary_parent: PathBuf,
}

impl ControlPlaneConfig {
    #[must_use]
    pub fn local(data_directory: impl AsRef<Path>) -> Self {
        let data_directory = data_directory.as_ref().to_path_buf();
        Self {
            temporary_parent: data_directory.join(".control-plane-runtime"),
            data_directory,
        }
    }

    /// Overrides the parent under which the instance-owned temporary root is created.
    #[must_use]
    pub fn with_temporary_parent(mut self, temporary_parent: impl AsRef<Path>) -> Self {
        self.temporary_parent = temporary_parent.as_ref().to_path_buf();
        self
    }

    #[must_use]
    pub fn data_directory(&self) -> &Path {
        &self.data_directory
    }

    #[must_use]
    pub fn temporary_parent(&self) -> &Path {
        &self.temporary_parent
    }
}

/// Error returned by an event publisher adapter.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EventPublishError {
    message: String,
}

impl EventPublishError {
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for EventPublishError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for EventPublishError {}

/// Event transport owned and closed by the Control Plane lifecycle.
pub trait EventPublisher: Send {
    /// Publishes one durable event. Implementations must deduplicate by
    /// `event.event_id` because a crash after publish and before acknowledgement
    /// can cause the same outbox event to be offered again.
    ///
    /// # Errors
    ///
    /// Returns a transport-specific failure without acknowledging the event.
    fn publish(&mut self, event: &OutboxEvent) -> Result<(), EventPublishError>;

    /// Closes the event transport and releases its resources.
    ///
    /// # Errors
    ///
    /// Returns a transport-specific failure if deterministic close fails.
    fn close(&mut self) -> Result<(), EventPublishError> {
        Ok(())
    }
}

/// Failure while draining the durable outbox.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OutboxError {
    Publish(EventPublishError),
    Acknowledge(StorageError),
    Audit(winwincode_audit::AuditError),
}

impl fmt::Display for OutboxError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Publish(error) => write!(formatter, "event publication failed: {error}"),
            Self::Acknowledge(error) => {
                write!(
                    formatter,
                    "event publication acknowledgement failed: {error}"
                )
            }
            Self::Audit(error) => write!(formatter, "audit event flush failed: {error}"),
        }
    }
}

impl std::error::Error for OutboxError {}

/// Control Plane startup failure. All resources are closed before this is returned.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StartError {
    message: String,
}

impl StartError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for StartError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for StartError {}

/// Control Plane commit failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CommitError {
    /// No state was committed.
    Storage(StorageError),
    /// State and outbox were committed, but the event remains pending.
    PublicationPending {
        receipt: Box<CommitReceipt>,
        source: OutboxError,
    },
}

impl CommitError {
    #[must_use]
    pub fn committed_receipt(&self) -> Option<&CommitReceipt> {
        match self {
            Self::Storage(_) => None,
            Self::PublicationPending { receipt, .. } => Some(receipt.as_ref()),
        }
    }
}

impl fmt::Display for CommitError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Storage(error) => write!(formatter, "state commit failed: {error}"),
            Self::PublicationPending { source, .. } => write!(
                formatter,
                "state committed, but its outbox event remains pending: {source}"
            ),
        }
    }
}

impl std::error::Error for CommitError {}

/// Failure of the canonical base Delivery command transaction.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DeliveryCommandCommitError {
    /// No durable member committed because command or storage validation failed.
    Storage(StorageError),
    /// The scoped Delivery required by a mutation does not exist.
    NotFound { delivery_id: DeliveryId },
    /// A different scoped request tried to create an existing Delivery.
    AlreadyExists { delivery_id: DeliveryId },
    /// The atomic transaction committed and only publication remains pending.
    PublicationPending {
        receipt: Box<CommitReceipt>,
        source: OutboxError,
    },
}

impl DeliveryCommandCommitError {
    #[must_use]
    pub fn public_code(&self) -> winwincode_api::generated::ErrorCode {
        use winwincode_api::generated::ErrorCode;
        match self {
            Self::NotFound { .. } => ErrorCode::ResourceNotFound,
            Self::AlreadyExists { .. } => ErrorCode::WrongState,
            Self::PublicationPending { .. } => ErrorCode::ServiceUnavailable,
            Self::Storage(error) => match error.kind() {
                StorageErrorKind::InvalidInput => ErrorCode::InvalidRequest,
                StorageErrorKind::RevisionConflict => ErrorCode::RevisionConflict,
                StorageErrorKind::RequestConflict => ErrorCode::IdempotencyConflict,
                StorageErrorKind::JournalNotFound => ErrorCode::ResourceNotFound,
                StorageErrorKind::RequestReplayMissing
                | StorageErrorKind::JournalAlreadyExists
                | StorageErrorKind::JournalConflict
                | StorageErrorKind::EventCursorExpired
                | StorageErrorKind::Adapter
                | StorageErrorKind::Closed => ErrorCode::ServiceUnavailable,
            },
        }
    }

    #[must_use]
    pub fn public_details(&self) -> winwincode_api::generated::ErrorDetails {
        use winwincode_api::generated::ErrorDetailValue;
        let mut details = winwincode_api::generated::ErrorDetails::new();
        if let Self::NotFound { delivery_id } | Self::AlreadyExists { delivery_id } = self {
            details.insert(
                "field".to_owned(),
                ErrorDetailValue::Variant4("deliveryId".to_owned()),
            );
            details.insert(
                "deliveryId".to_owned(),
                ErrorDetailValue::Variant4(delivery_id.0.clone()),
            );
        }
        details
    }

    #[must_use]
    pub const fn retryable(&self) -> bool {
        matches!(self, Self::PublicationPending { .. })
            || matches!(self, Self::Storage(error) if matches!(
                error.kind(),
                StorageErrorKind::RequestReplayMissing
                    | StorageErrorKind::JournalAlreadyExists
                    | StorageErrorKind::JournalConflict
                    | StorageErrorKind::EventCursorExpired
                    | StorageErrorKind::Adapter
                    | StorageErrorKind::Closed
            ))
    }

    #[must_use]
    pub fn committed_receipt(&self) -> Option<&CommitReceipt> {
        match self {
            Self::PublicationPending { receipt, .. } => Some(receipt),
            Self::Storage(_) | Self::NotFound { .. } | Self::AlreadyExists { .. } => None,
        }
    }
}

impl From<StorageError> for DeliveryCommandCommitError {
    fn from(error: StorageError) -> Self {
        Self::Storage(error)
    }
}

impl fmt::Display for DeliveryCommandCommitError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Storage(error) => write!(formatter, "Delivery command failed: {error}"),
            Self::NotFound { delivery_id } => {
                write!(formatter, "Delivery {} was not found", delivery_id.0)
            }
            Self::AlreadyExists { delivery_id } => {
                write!(formatter, "Delivery {} already exists", delivery_id.0)
            }
            Self::PublicationPending { source, .. } => write!(
                formatter,
                "Delivery command committed, but publication remains pending: {source}"
            ),
        }
    }
}

impl std::error::Error for DeliveryCommandCommitError {}

/// Failure of the specialized atomic Delivery verdict command.
#[derive(Debug)]
pub enum DeliveryVerdictCommitError {
    /// Sealed candidate, verification, or Evidence facts were stale or invalid.
    Coordination(CoordinationError),
    /// No Delivery journal, state, receipt, or event fact committed.
    Storage(StorageError),
    /// The complete transaction committed; publication remains in the outbox.
    PublicationPending {
        receipt: Box<CommitReceipt>,
        source: OutboxError,
    },
}

impl DeliveryVerdictCommitError {
    #[must_use]
    pub fn committed_receipt(&self) -> Option<&CommitReceipt> {
        match self {
            Self::PublicationPending { receipt, .. } => Some(receipt),
            Self::Coordination(_) | Self::Storage(_) => None,
        }
    }
}

impl fmt::Display for DeliveryVerdictCommitError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Coordination(error) => write!(formatter, "verdict computation failed: {error}"),
            Self::Storage(error) => write!(formatter, "verdict transaction failed: {error}"),
            Self::PublicationPending { source, .. } => write!(
                formatter,
                "verdict transaction committed, but its event remains pending: {source}"
            ),
        }
    }
}

impl std::error::Error for DeliveryVerdictCommitError {}

impl From<StorageError> for DeliveryVerdictCommitError {
    fn from(error: StorageError) -> Self {
        Self::Storage(error)
    }
}

/// Successful deterministic shutdown facts.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ShutdownReport {
    pub published_event_count: usize,
}

/// Shutdown failure. The lifecycle still attempts every close step.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ShutdownError {
    failures: Vec<String>,
}

impl ShutdownError {
    #[must_use]
    pub fn failures(&self) -> &[String] {
        &self.failures
    }
}

impl fmt::Display for ShutdownError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "Control Plane shutdown had {} failure(s): {}",
            self.failures.len(),
            self.failures.join("; ")
        )
    }
}

impl std::error::Error for ShutdownError {}

/// Running Control Plane application host.
///
/// The host is deliberately synchronous in this phase: it does not detach
/// background tasks, so shutdown has a finite and observable ownership chain.
pub struct ControlPlane {
    storage: Option<Box<dyn ProductStateStorage>>,
    local_database_path: Option<PathBuf>,
    audit_store: Option<AuditStore>,
    artifact_store: Option<ArtifactStore>,
    git_source_resolver: Option<Box<dyn GitSourceResolver>>,
    git_repository_root: Option<PathBuf>,
    publisher: Option<Box<dyn EventPublisher>>,
    temporary_root: Option<OwnedTemporaryRoot>,
    strongflow_sources: Option<strongflow_projection::StrongFlowProjectionSources>,
    delivery_authority: Option<Box<dyn DeliveryAuthorityPort>>,
    delivery_dispatcher: Option<Box<dyn ExecutionJobDispatcher>>,
    publication_authority: Option<Box<dyn PublicationAuthorityPort>>,
    publication_providers: Option<Box<dyn PublicationProviderRegistry>>,
}

impl ControlPlane {
    /// Opens and migrates the local `SQLite` database, replays durable outbox
    /// events, and only then returns a running Control Plane.
    ///
    /// # Errors
    ///
    /// Returns [`StartError`] after closing owned resources when storage open,
    /// migration, or durable outbox replay fails.
    #[allow(
        clippy::too_many_lines,
        reason = "startup keeps each owned resource's failure cleanup explicit"
    )]
    pub fn start_local(
        config: ControlPlaneConfig,
        mut publisher: Box<dyn EventPublisher>,
    ) -> Result<Self, StartError> {
        let ControlPlaneConfig {
            data_directory,
            temporary_parent,
        } = config;
        let temporary_root = match OwnedTemporaryRoot::create(&temporary_parent) {
            Ok(temporary_root) => temporary_root,
            Err(error) => {
                let cleanup = close_publisher(&mut publisher);
                return Err(StartError::new(format!(
                    "failed to create the owned temporary root: {error}{cleanup}"
                )));
            }
        };
        let storage = match SqliteStorage::open(&data_directory) {
            Ok(storage) => storage,
            Err(error) => {
                let mut cleanup_failures = Vec::new();
                if let Err(close_error) = publisher.close() {
                    cleanup_failures
                        .push(format!("event publisher close also failed: {close_error}"));
                }
                if let Err(release_error) = temporary_root.release() {
                    cleanup_failures.push(format!(
                        "temporary root release also failed: {release_error}"
                    ));
                }
                let cleanup = cleanup_suffix(&cleanup_failures);
                return Err(StartError::new(format!(
                    "failed to open Control Plane storage: {error}{cleanup}"
                )));
            }
        };
        let audit_store = match AuditStore::open(data_directory.join("audit")) {
            Ok(store) => store,
            Err(error) => {
                let mut cleanup_failures = Vec::new();
                if let Err(close_error) = Box::new(storage).close() {
                    cleanup_failures.push(format!("storage close also failed: {close_error}"));
                }
                if let Err(close_error) = publisher.close() {
                    cleanup_failures
                        .push(format!("event publisher close also failed: {close_error}"));
                }
                if let Err(release_error) = temporary_root.release() {
                    cleanup_failures.push(format!(
                        "temporary root release also failed: {release_error}"
                    ));
                }
                return Err(StartError::new(format!(
                    "failed to open immutable audit storage: {error}{}",
                    cleanup_suffix(&cleanup_failures)
                )));
            }
        };
        let object_store = match LocalArtifactObjectStore::open(data_directory.join("artifacts")) {
            Ok(store) => store,
            Err(error) => {
                let mut cleanup_failures = Vec::new();
                if let Err(close_error) = Box::new(storage).close() {
                    cleanup_failures.push(format!("storage close also failed: {close_error}"));
                }
                if let Err(close_error) = audit_store.close() {
                    cleanup_failures.push(format!("audit store close also failed: {close_error}"));
                }
                if let Err(close_error) = publisher.close() {
                    cleanup_failures
                        .push(format!("event publisher close also failed: {close_error}"));
                }
                if let Err(release_error) = temporary_root.release() {
                    cleanup_failures.push(format!(
                        "temporary root release also failed: {release_error}"
                    ));
                }
                return Err(StartError::new(format!(
                    "failed to open local Artifact object storage: {error}{}",
                    cleanup_suffix(&cleanup_failures)
                )));
            }
        };
        let artifact_store = match ArtifactStore::open(
            data_directory.join("artifact-catalog"),
            Box::new(object_store),
        ) {
            Ok(store) => store,
            Err(error) => {
                let mut cleanup_failures = Vec::new();
                if let Err(close_error) = Box::new(storage).close() {
                    cleanup_failures.push(format!("storage close also failed: {close_error}"));
                }
                if let Err(close_error) = audit_store.close() {
                    cleanup_failures.push(format!("audit store close also failed: {close_error}"));
                }
                if let Err(close_error) = publisher.close() {
                    cleanup_failures
                        .push(format!("event publisher close also failed: {close_error}"));
                }
                if let Err(release_error) = temporary_root.release() {
                    cleanup_failures.push(format!(
                        "temporary root release also failed: {release_error}"
                    ));
                }
                return Err(StartError::new(format!(
                    "failed to open Artifact metadata catalog: {error}{}",
                    cleanup_suffix(&cleanup_failures)
                )));
            }
        };
        let local_database_path = storage.database_path().to_path_buf();
        Self::start_with_resources(
            Box::new(storage),
            Some(local_database_path),
            Some(audit_store),
            Some(artifact_store),
            publisher,
            temporary_root,
        )
    }

    /// Composes the application with a storage adapter at the `PostgreSQL`-ready seam.
    ///
    /// # Errors
    ///
    /// Returns [`StartError`] after closing both adapters when durable outbox
    /// replay fails.
    pub fn start(
        storage: Box<dyn ProductStateStorage>,
        mut publisher: Box<dyn EventPublisher>,
    ) -> Result<Self, StartError> {
        let temporary_parent = std::env::temp_dir().join("winwincode-control-plane");
        let temporary_root = match OwnedTemporaryRoot::create(temporary_parent) {
            Ok(temporary_root) => temporary_root,
            Err(error) => {
                let mut failures = Vec::new();
                if let Err(close_error) = publisher.close() {
                    failures.push(format!("event publisher close also failed: {close_error}"));
                }
                if let Err(close_error) = storage.close() {
                    failures.push(format!("storage close also failed: {close_error}"));
                }
                return Err(StartError::new(format!(
                    "failed to create the owned temporary root: {error}{}",
                    cleanup_suffix(&failures)
                )));
            }
        };
        Self::start_with_resources(storage, None, None, None, publisher, temporary_root)
    }

    /// Composes the application with explicit product-state and Artifact adapters.
    ///
    /// # Errors
    ///
    /// Returns [`StartError`] after closing every owned adapter when startup
    /// outbox replay fails.
    pub fn start_with_artifacts(
        storage: Box<dyn ProductStateStorage>,
        artifact_store: ArtifactStore,
        mut publisher: Box<dyn EventPublisher>,
    ) -> Result<Self, StartError> {
        let temporary_parent = std::env::temp_dir().join("winwincode-control-plane");
        let temporary_root = match OwnedTemporaryRoot::create(temporary_parent) {
            Ok(temporary_root) => temporary_root,
            Err(error) => {
                let mut failures = Vec::new();
                if let Err(close_error) = publisher.close() {
                    failures.push(format!("event publisher close also failed: {close_error}"));
                }
                if let Err(close_error) = storage.close() {
                    failures.push(format!("storage close also failed: {close_error}"));
                }
                if let Err(close_error) = artifact_store.close() {
                    failures.push(format!("Artifact store close also failed: {close_error}"));
                }
                return Err(StartError::new(format!(
                    "failed to create the owned temporary root: {error}{}",
                    cleanup_suffix(&failures)
                )));
            }
        };
        Self::start_with_resources(
            storage,
            None,
            None,
            Some(artifact_store),
            publisher,
            temporary_root,
        )
    }

    fn start_with_resources(
        storage: Box<dyn ProductStateStorage>,
        local_database_path: Option<PathBuf>,
        audit_store: Option<AuditStore>,
        artifact_store: Option<ArtifactStore>,
        publisher: Box<dyn EventPublisher>,
        temporary_root: OwnedTemporaryRoot,
    ) -> Result<Self, StartError> {
        let mut control_plane = Self {
            storage: Some(storage),
            local_database_path,
            audit_store,
            artifact_store,
            git_source_resolver: None,
            git_repository_root: None,
            publisher: Some(publisher),
            temporary_root: Some(temporary_root),
            strongflow_sources: None,
            delivery_authority: None,
            delivery_dispatcher: None,
            publication_authority: None,
            publication_providers: None,
        };
        if let Err(error) = control_plane.flush_outbox() {
            let cleanup_failures = control_plane.close_resources();
            let cleanup = if cleanup_failures.is_empty() {
                String::new()
            } else {
                format!("; cleanup also failed: {}", cleanup_failures.join("; "))
            };
            return Err(StartError::new(format!(
                "failed to replay the durable outbox before startup: {error}{cleanup}"
            )));
        }
        Ok(control_plane)
    }

    /// Returns the canonical local product-state database owned by this host.
    ///
    /// Hosts composed through adapter injection do not claim a local database
    /// identity and return `None`.
    #[must_use]
    pub fn local_database_path(&self) -> Option<&Path> {
        self.local_database_path.as_deref()
    }

    /// Returns the instance-owned temporary root while the host is running.
    ///
    /// # Panics
    ///
    /// Panics only if the internal lifecycle invariant is broken and a running
    /// host has already lost ownership of its temporary root. Shutdown consumes
    /// the host, so callers cannot observe a normally released root through this
    /// method.
    #[must_use]
    pub fn temporary_root(&self) -> &Path {
        self.temporary_root
            .as_ref()
            .expect("a running Control Plane always owns a temporary root")
            .path()
            .expect("a running Control Plane must retain a healthy temporary-root lease")
    }

    /// Installs the trusted runtime-ledger and publication read adapters before
    /// the typed `StrongFlow` query port is exposed to a transport.
    ///
    /// # Errors
    ///
    /// Returns an error if an adapter set was already installed. Replacing a
    /// live authority would make previously issued read cursors ambiguous.
    pub fn install_strongflow_projection_sources(
        &mut self,
        sources: strongflow_projection::StrongFlowProjectionSources,
    ) -> Result<(), strongflow_projection::StrongFlowProjectionError> {
        if self.strongflow_sources.is_some() {
            return Err(
                strongflow_projection::StrongFlowProjectionError::invalid_request(
                    "StrongFlow projection sources are already installed",
                ),
            );
        }
        self.strongflow_sources = Some(sources);
        Ok(())
    }

    /// Installs the single trusted source resolver used to reconstruct Git
    /// candidate identities from complete Artifact bytes.
    ///
    /// # Errors
    ///
    /// Rejects replacement of a live resolver so one process cannot reinterpret
    /// an already accepted Artifact through another source authority.
    pub fn install_git_source_resolver(
        &mut self,
        resolver: Box<dyn GitSourceResolver>,
    ) -> Result<(), CandidateResolutionError> {
        if self.git_source_resolver.is_some() {
            return Err(CandidateResolutionError::Storage(
                StorageError::invalid_input("Git source resolver is already installed"),
            ));
        }
        self.git_repository_root = resolver.controlled_repository_root().map(Path::to_path_buf);
        self.git_source_resolver = Some(resolver);
        Ok(())
    }

    /// Installs the controlled repository root used by durable candidate Git
    /// references.  Local source resolvers provide this automatically; this
    /// setter is for a resolver adapter whose repository root is configured by
    /// the application rather than exposed by the adapter itself.
    ///
    /// # Errors
    ///
    /// Rejects a missing/non-directory root or replacement of an already
    /// selected retention root.
    pub fn install_git_repository_root(
        &mut self,
        root: impl AsRef<Path>,
    ) -> Result<(), CandidateResolutionError> {
        let root = std::fs::canonicalize(root.as_ref()).map_err(|_| {
            CandidateResolutionError::Storage(StorageError::invalid_input(
                "controlled Git repository root is unavailable",
            ))
        })?;
        if !root.is_dir() {
            return Err(CandidateResolutionError::Storage(
                StorageError::invalid_input("controlled Git repository root is not a directory"),
            ));
        }
        if self
            .git_repository_root
            .as_ref()
            .is_some_and(|existing| existing != &root)
        {
            return Err(CandidateResolutionError::Storage(
                StorageError::invalid_input("controlled Git repository root is already installed"),
            ));
        }
        self.git_repository_root = Some(root);
        Ok(())
    }

    /// Returns the one controlled repository root selected for candidate
    /// reconstruction and durable Git retention.
    #[must_use]
    pub fn git_repository_root(&self) -> Option<&Path> {
        self.git_repository_root.as_deref()
    }

    /// Commits one canonical HTTP command's state and outbox first, then
    /// publishes pending events.
    ///
    /// # Errors
    ///
    /// Returns [`CommitError::Storage`] when nothing was committed, or
    /// [`CommitError::PublicationPending`] when state is durable and its event
    /// must be replayed.
    pub fn commit(
        &mut self,
        command: &CommandEnvelope,
        change: StateChange,
    ) -> Result<CommitReceipt, CommitError> {
        if delivery_command(&command.command)
            || change.stream_id.starts_with("delivery:")
            || change.stream_id.starts_with("runtime:")
        {
            return Err(CommitError::Storage(StorageError::invalid_input(
                "Delivery and runtime state streams require a typed atomic transaction",
            )));
        }
        if change.events.iter().any(|event| {
            event.projection_stream().is_some()
                || reserved_public_projection_topic(&event.topic)
                || reserved_delivery_transaction_topic(&event.topic)
        }) {
            return Err(CommitError::Storage(StorageError::invalid_input(
                "Delivery and public projection events require a typed Control Plane transaction",
            )));
        }
        let commit = storage_commit(command, change).map_err(CommitError::Storage)?;
        let receipt = self
            .storage_mut()
            .map_err(CommitError::Storage)?
            .commit(&commit)
            .map_err(CommitError::Storage)?;
        drop(commit);
        self.flush_outbox()
            .map_err(|source| CommitError::PublicationPending {
                receipt: Box::new(receipt.clone()),
                source,
            })?;
        Ok(receipt)
    }

    /// Atomically commits one canonical non-dispatch Delivery command and its
    /// public Delivery event through the single typed Delivery transaction.
    ///
    /// # Errors
    ///
    /// Returns before persistence for an unsupported command, invalid scope,
    /// stale revision, or failed atomic member. Publication failure retains
    /// the committed event for startup replay.
    pub fn commit_delivery_command(
        &mut self,
        command: &CommandEnvelope,
        facts: &DeliveryCommandFacts,
    ) -> Result<CommitReceipt, DeliveryCommandCommitError> {
        let receipt = {
            let storage = self
                .storage_mut()
                .map_err(DeliveryCommandCommitError::Storage)?;
            delivery_command_transaction::execute(storage, command, facts, None)?
        };
        self.flush_outbox()
            .map_err(|source| DeliveryCommandCommitError::PublicationPending {
                receipt: Box::new(receipt.clone()),
                source,
            })?;
        Ok(receipt)
    }

    fn commit_delivery_command_with_handoff(
        &mut self,
        command: &CommandEnvelope,
        facts: &DeliveryCommandFacts,
        handoff: &terminal_outcome_transaction::DeliveryTerminalHandoff,
    ) -> Result<CommitReceipt, DeliveryCommandCommitError> {
        let receipt = {
            let storage = self
                .storage_mut()
                .map_err(DeliveryCommandCommitError::Storage)?;
            delivery_command_transaction::execute(storage, command, facts, Some(handoff))?
        };
        self.flush_outbox()
            .map_err(|source| DeliveryCommandCommitError::PublicationPending {
                receipt: Box::new(receipt.clone()),
                source,
            })?;
        Ok(receipt)
    }

    /// Atomically commits one `delivery.advance` journal record, canonical
    /// snapshot, scoped command receipt, and execution-job outbox intent before
    /// offering the exact committed job to the `ExecutionPort` dispatcher.
    ///
    /// # Errors
    ///
    /// Returns without dispatch when any pre-commit member fails. A dispatch or
    /// acknowledgement failure carries the committed receipt and leaves the
    /// durable event pending for startup replay.
    pub fn commit_delivery_execution(
        &mut self,
        command: &CommandEnvelope,
        pending: &PendingDeliveryExecution,
        dispatcher: &mut dyn ExecutionJobDispatcher,
    ) -> Result<DeliveryExecutionDispatchReceipt, DeliveryExecutionError> {
        let receipt = {
            let storage = self.storage_mut().map_err(|error| {
                DeliveryExecutionError::Commit(DeliveryExecutionPortError::new(error.to_string()))
            })?;
            delivery_transaction::execute(storage, command, pending, dispatcher, None)?
        };
        self.flush_outbox().map_err(|source| {
            DeliveryExecutionError::ProjectionPublicationAfterDispatch {
                commit: Box::new(receipt.commit.clone()),
                source: DeliveryExecutionPortError::new(source.to_string()),
            }
        })?;
        Ok(receipt)
    }

    fn commit_delivery_execution_with_handoff(
        &mut self,
        command: &CommandEnvelope,
        pending: &PendingDeliveryExecution,
        dispatcher: &mut dyn ExecutionJobDispatcher,
        handoff: &terminal_outcome_transaction::DeliveryTerminalHandoff,
    ) -> Result<DeliveryExecutionDispatchReceipt, DeliveryExecutionError> {
        let receipt = {
            let storage = self.storage_mut().map_err(|error| {
                DeliveryExecutionError::Commit(DeliveryExecutionPortError::new(error.to_string()))
            })?;
            delivery_transaction::execute(storage, command, pending, dispatcher, Some(handoff))?
        };
        self.flush_outbox().map_err(|source| {
            DeliveryExecutionError::ProjectionPublicationAfterDispatch {
                commit: Box::new(receipt.commit.clone()),
                source: DeliveryExecutionPortError::new(source.to_string()),
            }
        })?;
        Ok(receipt)
    }

    /// Persists one authoritative Worker `session.binding` message as the two
    /// canonical Delivery mutations that attach its `WorkerSession` and then its
    /// `CodexThread`.
    ///
    /// The generated message is the only wire input. The second argument is an
    /// opaque scheduler-owned `SessionBinding` authority, and `server_time` is
    /// captured from the trusted ingress clock. The message cannot authorize
    /// itself or backdate `sentAt` to reopen an expired lease.
    ///
    /// # Errors
    ///
    /// Returns before the first write for a foreign job, stale binding, lease
    /// mismatch, or malformed message. If the `WorkerSession` phase committed
    /// but the `CodexThread` phase failed, the returned error carries the first
    /// durable receipt so an exact retry can continue receipt-first.
    pub fn commit_delivery_session_binding(
        &mut self,
        message: &execution_port::SessionBindingMessage,
        authority: &winwincode_delivery::application::stage::SessionBindingAuthority,
        server_time: &Instant,
    ) -> Result<DeliverySessionBindingCommitReceipt, DeliverySessionBindingCommitError> {
        let commit = {
            let storage = self
                .storage_mut()
                .map_err(DeliverySessionBindingCommitError::Storage)?;
            session_binding_transaction::execute_at(storage, message, authority, server_time)?
        };
        self.flush_outbox().map_err(|source| {
            DeliverySessionBindingCommitError::PublicationPending {
                commit: Box::new(commit.clone()),
                source,
            }
        })?;
        Ok(commit)
    }

    /// Accepts one generated Worker `runtime.event` only after joining its
    /// durable Delivery-stage `ExecutionJob` to the current four-identity
    /// `SessionBinding`. `server_time` is the trusted ingress clock; Worker
    /// `sentAt` remains only a causal/audit fact. Rejected input returns a
    /// stable generated ack and does not write Delivery state, journal,
    /// receipt, or outbox members.
    ///
    /// # Errors
    ///
    /// Returns a storage error when the runtime ledger cannot be read or
    /// committed, or a publication-pending error when an accepted ack was
    /// committed but its outbox notification still needs flushing.
    pub fn accept_runtime_event(
        &mut self,
        scope: &RepositoryScope,
        message: &execution_port::RuntimeEventMessage,
        authority: &winwincode_delivery::application::stage::SessionBindingAuthority,
        server_time: &Instant,
    ) -> Result<execution_port::RuntimeAckMessage, RuntimeMessageError> {
        let ack = {
            let storage = self.storage_mut().map_err(RuntimeMessageError::Storage)?;
            runtime_event_transaction::execute_at(storage, scope, message, authority, server_time)?
        };
        if !matches!(
            ack.status,
            execution_port::LeaseWriteStatus::Accepted
                | execution_port::LeaseWriteStatus::Duplicate
        ) {
            return Ok(ack);
        }
        self.flush_outbox()
            .map_err(|source| RuntimeMessageError::PublicationPending {
                ack: Box::new(ack.clone()),
                source,
            })?;
        Ok(ack)
    }

    /// Accepts one generated `artifact.open` after exact durable Job, scope,
    /// `SessionBinding`, and scheduler authority validation.
    ///
    /// # Errors
    ///
    /// Returns before metadata persistence for a foreign/stale lease, missing
    /// durable Job, incomplete `SessionBinding`, or invalid immutable descriptor.
    pub fn accept_artifact_open(
        &mut self,
        scope: &RepositoryScope,
        message: &execution_port::ArtifactOpenMessage,
        authority: &winwincode_delivery::application::stage::SessionBindingAuthority,
    ) -> Result<execution_port::ArtifactAckMessage, ArtifactMessageError> {
        let data_directory = self.enterprise_quota_data_directory()?;
        let mut quota = DurableEnterpriseQuotaAdmission::new(
            SqliteStorage::open(&data_directory).map_err(ArtifactMessageError::Storage)?,
        );
        let mut usage = DurableArtifactEnterpriseUsage::new(
            SqliteStorage::open(&data_directory).map_err(ArtifactMessageError::Storage)?,
        );
        let result = {
            let storage = self.storage.as_deref().ok_or_else(|| {
                ArtifactMessageError::Storage(StorageError::adapter(
                    "Control Plane storage is closed",
                ))
            })?;
            let artifacts = self.artifact_store.as_mut().ok_or_else(|| {
                ArtifactMessageError::Storage(StorageError::adapter(
                    "Control Plane Artifact store is not configured",
                ))
            })?;
            let mut enterprise_quota = ArtifactEnterpriseQuotaSaga::new(&mut quota, &mut usage);
            artifact_transaction::accept_open(
                storage,
                artifacts,
                scope,
                message,
                authority,
                &mut enterprise_quota,
            )
        };
        let usage_close = usage.close();
        let quota_close = quota.close();
        let ack = result?;
        usage_close.map_err(ArtifactMessageError::Storage)?;
        quota_close.map_err(ArtifactMessageError::Storage)?;
        Ok(ack)
    }

    /// Accepts one generated `artifact.chunk` through the same lease-scoped
    /// authority and content-addressed Artifact interface as `artifact.open`.
    ///
    /// # Errors
    ///
    /// Returns without accepting bytes for a gap, changed chunk, invalid
    /// digest/base64 payload, stale lease, or foreign Artifact identity.
    pub fn accept_artifact_chunk(
        &mut self,
        scope: &RepositoryScope,
        message: &execution_port::ArtifactChunkMessage,
        authority: &winwincode_delivery::application::stage::SessionBindingAuthority,
    ) -> Result<execution_port::ArtifactAckMessage, ArtifactMessageError> {
        let data_directory = self.enterprise_quota_data_directory()?;
        let mut quota = DurableEnterpriseQuotaAdmission::new(
            SqliteStorage::open(&data_directory).map_err(ArtifactMessageError::Storage)?,
        );
        let mut usage = DurableArtifactEnterpriseUsage::new(
            SqliteStorage::open(&data_directory).map_err(ArtifactMessageError::Storage)?,
        );
        let result = {
            let storage = self.storage.as_deref().ok_or_else(|| {
                ArtifactMessageError::Storage(StorageError::adapter(
                    "Control Plane storage is closed",
                ))
            })?;
            let artifacts = self.artifact_store.as_mut().ok_or_else(|| {
                ArtifactMessageError::Storage(StorageError::adapter(
                    "Control Plane Artifact store is not configured",
                ))
            })?;
            let mut enterprise_quota = ArtifactEnterpriseQuotaSaga::new(&mut quota, &mut usage);
            artifact_transaction::accept_chunk(
                storage,
                artifacts,
                scope,
                message,
                authority,
                &mut enterprise_quota,
            )
        };
        let usage_close = usage.close();
        let quota_close = quota.close();
        let ack = result?;
        usage_close.map_err(ArtifactMessageError::Storage)?;
        quota_close.map_err(ArtifactMessageError::Storage)?;
        if message.is_final
            && matches!(
                ack.status,
                execution_port::LeaseWriteStatus::Accepted
                    | execution_port::LeaseWriteStatus::Duplicate
            )
            && self.git_source_resolver.is_some()
            && self.git_repository_root.is_some()
        {
            self.pin_candidate_git_after_final_artifact_ack(scope, message, &ack, authority)
                .map_err(|error| {
                    ArtifactMessageError::Storage(StorageError::adapter(error.to_string()))
                })?;
        }
        Ok(ack)
    }

    fn enterprise_quota_data_directory(&self) -> Result<PathBuf, ArtifactMessageError> {
        self.local_enterprise_quota_directory()
            .map_err(ArtifactMessageError::Storage)
    }

    fn local_enterprise_quota_directory(&self) -> Result<PathBuf, StorageError> {
        self.local_database_path
            .as_deref()
            .and_then(Path::parent)
            .map(Path::to_path_buf)
            .ok_or_else(|| {
                StorageError::adapter(
                    "enterprise quota requires canonical local Control Plane storage",
                )
            })
    }

    fn release_terminal_artifact_quota(
        &mut self,
        message: &execution_port::JobOutcomeMessage,
        status: winwincode_delivery::application::stage::TerminalOutcomeStatus,
    ) -> Result<(), ArtifactEnterpriseQuotaSagaError> {
        let data_directory = self.local_enterprise_quota_directory()?;
        let mut quota = DurableEnterpriseQuotaAdmission::new(SqliteStorage::open(&data_directory)?);
        let mut usage = DurableArtifactEnterpriseUsage::new(SqliteStorage::open(&data_directory)?);
        let reason = match status {
            winwincode_delivery::application::stage::TerminalOutcomeStatus::Cancelled => {
                winwincode_storage::EnterpriseQuotaReleaseReason::Cancelled
            }
            winwincode_delivery::application::stage::TerminalOutcomeStatus::Failed
            | winwincode_delivery::application::stage::TerminalOutcomeStatus::InfrastructureError => {
                winwincode_storage::EnterpriseQuotaReleaseReason::Failed
            }
            winwincode_delivery::application::stage::TerminalOutcomeStatus::Succeeded => {
                unreachable!("successful terminal outcomes do not release Artifact quota")
            }
        };
        let result = {
            let artifacts = self.artifact_store.as_ref().ok_or_else(|| {
                ArtifactEnterpriseQuotaSagaError::Storage(StorageError::adapter(
                    "Control Plane Artifact store is not configured",
                ))
            })?;
            ArtifactEnterpriseQuotaSaga::new(&mut quota, &mut usage)
                .release_unfinished_job(artifacts, &message.lease.job_id, reason, &message.sent_at)
                .map(|_| ())
        };
        let usage_close = usage.close();
        let quota_close = quota.close();
        result?;
        usage_close?;
        quota_close?;
        Ok(())
    }

    /// Rebuilds and freezes the current Delivery candidate from one exact,
    /// complete Artifact and its successful fenced Worker outcome.
    ///
    /// This is a derived read: it does not append Delivery state or let a
    /// caller supply commit/tree/diff/path identities.
    ///
    /// # Errors
    ///
    /// Rejects missing/foreign/corrupt Artifact bytes, mismatched scope or
    /// provenance, stale terminal facts, and source identities that differ
    /// from the current Delivery Spec.
    pub fn resolve_delivery_candidate(
        &self,
        scope: &RepositoryScope,
        delivery_id: &DeliveryId,
        artifact_id: &winwincode_domain::ArtifactId,
        artifact_digest: &Sha256Digest,
        terminal_facts: &winwincode_delivery::application::stage::DeliveryTerminalOutcomeFacts,
    ) -> Result<winwincode_delivery::domain::FrozenDeliveryCandidate, CandidateResolutionError>
    {
        let storage = self.storage.as_deref().ok_or_else(|| {
            CandidateResolutionError::Storage(StorageError::adapter(
                "Control Plane storage is closed",
            ))
        })?;
        let artifacts = self.artifact_store.as_ref().ok_or_else(|| {
            CandidateResolutionError::Storage(StorageError::adapter(
                "Control Plane Artifact store is not configured",
            ))
        })?;
        let resolver = self.git_source_resolver.as_deref().ok_or_else(|| {
            CandidateResolutionError::Storage(StorageError::adapter(
                "Control Plane Git source resolver is not configured",
            ))
        })?;
        candidate_source::resolve(
            storage,
            artifacts,
            resolver,
            scope,
            delivery_id,
            artifact_id,
            artifact_digest,
            terminal_facts,
        )
    }

    /// Pins a complete candidate Artifact after its final generated
    /// acknowledgement has been accepted.
    ///
    /// The acknowledgement and chunk are checked against the same sealed
    /// `SessionBinding` that accepted the upload.  Candidate source facts are
    /// rebuilt from the durable Artifact and controlled Git repository before
    /// the retention ledger creates its stable reference.  Calling this method
    /// for a non-final or non-candidate frame is a no-op; exact final retries
    /// return the existing durable pin.
    ///
    /// # Errors
    ///
    /// Returns before a pin is created for a changed acknowledgement, missing
    /// source, foreign Artifact, unavailable local retention storage, or a Git
    /// object/reference conflict.  Remote/custom resolver adapters can leave
    /// the repository root unset; those adapters must invoke their equivalent
    /// retention seam with a configured controlled root.
    #[allow(clippy::too_many_lines)]
    pub fn pin_candidate_git_after_final_artifact_ack(
        &mut self,
        scope: &RepositoryScope,
        chunk: &execution_port::ArtifactChunkMessage,
        acknowledgement: &execution_port::ArtifactAckMessage,
        authority: &winwincode_delivery::application::stage::SessionBindingAuthority,
    ) -> Result<Option<CandidateGitPinReceipt>, CandidateResolutionError> {
        if !chunk.is_final
            || chunk.artifact_id != acknowledgement.artifact_id
            || !matches!(
                acknowledgement.status,
                execution_port::LeaseWriteStatus::Accepted
                    | execution_port::LeaseWriteStatus::Duplicate
            )
            || acknowledgement.error.is_some()
            || acknowledgement.replay_from_sequence.is_some()
        {
            return Ok(None);
        }
        if acknowledgement.lease != chunk.lease
            || acknowledgement.worker_session_id != chunk.worker_session_id
            || acknowledgement.session_identity != chunk.session_identity
            || acknowledgement.ack_sequence.0 != chunk.sequence.0
            || acknowledgement.lease != authority_lease(authority)
        {
            return Err(CandidateResolutionError::Storage(
                StorageError::invalid_input(
                    "final Artifact acknowledgement differs from its sealed Worker authority",
                ),
            ));
        }
        let local_database_path = self.local_database_path.clone().ok_or_else(|| {
            CandidateResolutionError::Storage(StorageError::adapter(
                "candidate Git retention requires local Control Plane storage",
            ))
        })?;
        let repository_root = self.git_repository_root.clone().ok_or_else(|| {
            CandidateResolutionError::Storage(StorageError::adapter(
                "candidate Git retention repository root is not configured",
            ))
        })?;
        let source_resolver = self.git_source_resolver.as_deref().ok_or_else(|| {
            CandidateResolutionError::Storage(StorageError::adapter(
                "Control Plane Git source resolver is not configured",
            ))
        })?;
        let storage = self
            .storage_ref()
            .map_err(CandidateResolutionError::Storage)?;
        let (_, job) = delivery_transaction::load_durable_execution_job(
            storage,
            &acknowledgement.lease.job_id,
        )
        .map_err(CandidateResolutionError::Storage)?;
        let ExecutionScope::DeliveryStageExecutionScope(job_scope) = &job.scope else {
            return Err(CandidateResolutionError::Storage(
                StorageError::invalid_input(
                    "candidate Artifact retention requires a Delivery execution Job",
                ),
            ));
        };
        let provenance = candidate_source::provenance_from_session_binding(authority)?;
        let scope_key = repository_scope_key(scope)?;
        let artifacts = self.artifact_store.as_ref().ok_or_else(|| {
            CandidateResolutionError::Storage(StorageError::adapter(
                "Control Plane Artifact store is not configured",
            ))
        })?;
        let receipt = artifacts.complete_write_receipt(
            &scope_key,
            &acknowledgement.artifact_id,
            &provenance,
        )?;
        if receipt.record().kind() != "candidate" {
            return Ok(None);
        }
        if u64::try_from(acknowledgement.ack_sequence.0).ok()
            != Some(receipt.acknowledged_sequence())
        {
            return Err(CandidateResolutionError::Storage(
                StorageError::invalid_input(
                    "final Artifact acknowledgement sequence is not complete",
                ),
            ));
        }
        let source = candidate_source::resolve_source(
            storage,
            artifacts,
            source_resolver,
            scope,
            &job_scope.delivery_id,
            &acknowledgement.artifact_id,
            receipt.record().digest(),
            receipt.record().provenance().clone(),
        )?;
        let final_ack_digest = artifact_ack_digest(acknowledgement)?;
        let mut retention_storage =
            SqliteStorage::open(local_database_path.parent().ok_or_else(|| {
                CandidateResolutionError::Storage(StorageError::adapter(
                    "Control Plane database path has no parent",
                ))
            })?)
            .map_err(CandidateResolutionError::Storage)?;
        let pin = {
            let mut retention = retention_storage
                .git_candidate_retention(&repository_root)
                .map_err(CandidateResolutionError::GitRetention)?;
            retention
                .pin_after_final_artifact_ack(&receipt, &source, &final_ack_digest)
                .map_err(CandidateResolutionError::GitRetention)?
        };
        Box::new(retention_storage)
            .close()
            .map_err(CandidateResolutionError::Storage)?;
        Ok(Some(pin))
    }

    /// Durably records that a terminal Delivery and every candidate reader
    /// have closed.  The supplied terminal receipt must already be present in
    /// the canonical receipt table and must identify the current Delivery
    /// revision; this method is the only Control Plane constructor for the
    /// release authority used below.
    ///
    /// # Errors
    ///
    /// Returns an error when the terminal receipt is stale, foreign, or the
    /// read-closure mutation cannot be committed.
    pub fn commit_candidate_git_reads_closed(
        &mut self,
        delivery_id: &DeliveryId,
        terminal_receipt: &CommitReceipt,
        terminal_outcome: CandidateGitTerminalOutcome,
    ) -> Result<CandidateGitReadsClosedReceipt, CandidateResolutionError> {
        self.commit_candidate_git_reads_closed_with_guards(
            delivery_id,
            terminal_receipt,
            terminal_outcome,
            &[],
        )
    }

    fn commit_candidate_git_reads_closed_with_guards(
        &mut self,
        delivery_id: &DeliveryId,
        terminal_receipt: &CommitReceipt,
        terminal_outcome: CandidateGitTerminalOutcome,
        reader_guards: &[StateRevisionGuard],
    ) -> Result<CandidateGitReadsClosedReceipt, CandidateResolutionError> {
        let receipt = {
            let storage = self
                .storage_mut()
                .map_err(CandidateResolutionError::Storage)?;
            candidate_git_release::commit(
                storage,
                delivery_id,
                terminal_receipt,
                terminal_outcome,
                reader_guards,
            )?
        };
        self.flush_outbox().map_err(|error| {
            CandidateResolutionError::Storage(StorageError::adapter(error.to_string()))
        })?;
        Ok(receipt)
    }

    /// Releases a candidate Git reference using the exact durable Delivery
    /// terminal/read-closure receipt.  The receipt is checked again against
    /// canonical state so a stale or tampered in-memory value cannot authorize
    /// deletion.
    ///
    /// # Errors
    ///
    /// Returns an error when the read-closure receipt is missing or the
    /// resulting release authority fails durable validation.
    pub fn release_candidate_git_after_delivery_reads_closed(
        &mut self,
        pin: &CandidateGitPinReceipt,
        reads_closed: &CandidateGitReadsClosedReceipt,
    ) -> Result<CandidateGitReleaseReceipt, CandidateResolutionError> {
        let authority = reads_closed.release_authority()?;
        self.release_candidate_git_after_delivery_final(pin, &authority)
    }

    /// Releases a durable candidate Git reference only after the caller has
    /// sealed both the Delivery terminal receipt and the read-closure receipt.
    /// The release ledger is receipt-first and idempotent across retries and
    /// process restarts.
    ///
    /// # Errors
    ///
    /// Rejects a changed pin/terminal authority, a foreign repository, a moved
    /// reference, or an unavailable local retention database.
    pub fn release_candidate_git_after_delivery_final(
        &mut self,
        pin: &CandidateGitPinReceipt,
        authority: &CandidateGitReleaseAuthority,
    ) -> Result<CandidateGitReleaseReceipt, CandidateResolutionError> {
        candidate_git_release::validate_release_authority(
            self.storage_ref()
                .map_err(CandidateResolutionError::Storage)?,
            pin,
            authority,
        )?;
        let local_database_path = self.local_database_path.clone().ok_or_else(|| {
            CandidateResolutionError::Storage(StorageError::adapter(
                "candidate Git retention requires local Control Plane storage",
            ))
        })?;
        let repository_root = self.git_repository_root.clone().ok_or_else(|| {
            CandidateResolutionError::Storage(StorageError::adapter(
                "candidate Git retention repository root is not configured",
            ))
        })?;
        let parent = local_database_path.parent().ok_or_else(|| {
            CandidateResolutionError::Storage(StorageError::adapter(
                "Control Plane database path has no parent",
            ))
        })?;
        let mut retention_storage =
            SqliteStorage::open(parent).map_err(CandidateResolutionError::Storage)?;
        let release = {
            let mut retention = retention_storage
                .git_candidate_retention(&repository_root)
                .map_err(CandidateResolutionError::GitRetention)?;
            retention
                .release_after_delivery_final(pin, authority)
                .map_err(CandidateResolutionError::GitRetention)?
        };
        Box::new(retention_storage)
            .close()
            .map_err(CandidateResolutionError::Storage)?;
        Ok(release)
    }

    /// Completes the production terminal Delivery boundary for every
    /// candidate retained by that Delivery.
    ///
    /// The terminal/read-closure receipt is committed before any Git
    /// reference is touched.  Candidate bindings are loaded from the durable
    /// retention ledger, then each exact reference is released through the
    /// receipt-first compare-and-swap operation.  A failure after one or more
    /// releases leaves the closure and release intents durable; an exact
    /// Delivery command replay can therefore resume the remaining releases.
    ///
    /// This is intentionally a narrow Control Plane composition seam.  The
    /// Delivery application calls it only after a canonical terminal
    /// Delivery mutation (or its exact replay) has been loaded.  It does not
    /// accept caller-supplied commit/tree/reference identities.
    ///
    /// # Errors
    ///
    /// Returns before releasing any reference when the terminal receipt is
    /// stale or the Delivery still has an active reader.  A storage or Git
    /// failure after the read-closure commit is retryable because all prior
    /// receipts remain durable.
    pub fn finalize_candidate_git_after_delivery_terminal(
        &mut self,
        delivery_id: &DeliveryId,
        terminal_receipt: &CommitReceipt,
        terminal_outcome: CandidateGitTerminalOutcome,
    ) -> Result<Vec<CandidateGitReleaseReceipt>, CandidateResolutionError> {
        // Adapter-injected Control Planes without a local candidate-retention
        // root have no Git refs to release.  The local production composition
        // always installs both values before accepting a Delivery command.
        if self.git_source_resolver.is_none() && self.git_repository_root.is_none() {
            return Ok(Vec::new());
        }
        let Some(reader_guards) = candidate_git_release::ensure_publication_readers_closed(
            self.storage_ref()
                .map_err(CandidateResolutionError::Storage)?,
            delivery_id,
        )?
        else {
            // A configured publication target is a future reader even before
            // its durable Publication intent exists.  Keep every candidate
            // pin until that intent reaches a terminal state.
            return Ok(Vec::new());
        };
        let reads_closed = self.commit_candidate_git_reads_closed_with_guards(
            delivery_id,
            terminal_receipt,
            terminal_outcome,
            &reader_guards,
        )?;
        let pins = self.load_candidate_git_pins_for_delivery(delivery_id)?;
        let mut releases = Vec::with_capacity(pins.len());
        for pin in pins {
            releases
                .push(self.release_candidate_git_after_delivery_reads_closed(&pin, &reads_closed)?);
        }
        Ok(releases)
    }

    fn load_candidate_git_pins_for_delivery(
        &self,
        delivery_id: &DeliveryId,
    ) -> Result<Vec<CandidateGitPinReceipt>, CandidateResolutionError> {
        let local_database_path = self.local_database_path.clone().ok_or_else(|| {
            CandidateResolutionError::Storage(StorageError::adapter(
                "candidate Git retention requires local Control Plane storage",
            ))
        })?;
        let repository_root = self.git_repository_root.clone().ok_or_else(|| {
            CandidateResolutionError::Storage(StorageError::adapter(
                "candidate Git retention repository root is not configured",
            ))
        })?;
        let parent = local_database_path.parent().ok_or_else(|| {
            CandidateResolutionError::Storage(StorageError::adapter(
                "Control Plane database path has no parent",
            ))
        })?;
        let mut retention_storage =
            SqliteStorage::open(parent).map_err(CandidateResolutionError::Storage)?;
        let pins = {
            let mut retention = retention_storage
                .git_candidate_retention(&repository_root)
                .map_err(CandidateResolutionError::GitRetention)?;
            retention
                .load_by_delivery(delivery_id)
                .map_err(CandidateResolutionError::GitRetention)?
        };
        Box::new(retention_storage)
            .close()
            .map_err(CandidateResolutionError::Storage)?;
        Ok(pins)
    }

    /// Retries the production candidate release for a terminal Delivery by
    /// resolving its original durable mutation receipt from the canonical
    /// stream revision.  Publication recovery uses this narrow composition
    /// after a Published, Failed, or Cancelled Publication response, where
    /// the original Delivery command response is no longer in memory.
    pub(crate) fn finalize_candidate_git_for_terminal_delivery(
        &mut self,
        delivery_id: &DeliveryId,
    ) -> Result<(), CandidateResolutionError> {
        if self.git_source_resolver.is_none() && self.git_repository_root.is_none() {
            return Ok(());
        }
        let stream_id = delivery_transaction::delivery_stream_id(delivery_id);
        let receipt = {
            let storage = self
                .storage_ref()
                .map_err(CandidateResolutionError::Storage)?;
            let state = storage.load_state(&stream_id)?.ok_or_else(|| {
                CandidateResolutionError::Storage(StorageError::invalid_input(
                    "terminal Delivery state is missing",
                ))
            })?;
            let delivery = Delivery::decode_json(&state.payload).map_err(|error| {
                CandidateResolutionError::Storage(StorageError::invalid_input(format!(
                    "terminal Delivery state is invalid: {error}"
                )))
            })?;
            if delivery.id() != delivery_id
                || delivery.revision() != state.revision
                || delivery.snapshot().status
                    != winwincode_delivery::domain::DeliveryStatus::Delivered
            {
                return Ok(());
            }
            storage
                .load_receipt_for_stream_revision(&stream_id, state.revision)?
                .ok_or_else(|| {
                    CandidateResolutionError::Storage(StorageError::invalid_input(
                        "terminal Delivery revision has no durable command receipt",
                    ))
                })?
        };
        self.finalize_candidate_git_after_delivery_terminal(
            delivery_id,
            &receipt,
            CandidateGitTerminalOutcome::Delivered,
        )
        .map(|_| ())
    }

    /// Persists one lease-fenced Worker `job.outcome` through the only typed
    /// terminal Delivery transaction.
    ///
    /// Receipt replay is resolved before current Delivery, journal, durable job,
    /// or replacement authority is read. A new message is joined to its exact
    /// durable dispatch intent and opaque scheduler/Worker facts before the
    /// canonical terminal transition is committed. `server_time` is the
    /// trusted ingress clock and Worker `sentAt` cannot authorize the lease.
    ///
    /// # Errors
    ///
    /// Returns before persistence for a stale/foreign lease, binding, thread,
    /// sequence, Artifact, message time, or non-terminal stage transition.
    pub fn commit_delivery_terminal_outcome(
        &mut self,
        scope: &RepositoryScope,
        message: &execution_port::JobOutcomeMessage,
        facts: &winwincode_delivery::application::stage::DeliveryTerminalOutcomeFacts,
        server_time: &Instant,
    ) -> Result<DeliveryTerminalOutcomeCommitReceipt, DeliveryTerminalOutcomeCommitError> {
        let verifier_policy = terminal_outcome_transaction::verifier_policy_authority_at(
            self.storage_ref()
                .map_err(DeliveryTerminalOutcomeCommitError::Storage)?,
            scope,
            message,
            facts,
            server_time,
        )?;
        if let Some(authority) = verifier_policy {
            let directory = self.local_enterprise_quota_directory()?;
            let mut policy = DurableWorkerPolicyEnforcement::open(directory).map_err(|_| {
                DeliveryTerminalOutcomeCommitError::Storage(StorageError::adapter(
                    "Verifier enterprise Policy authority is unavailable",
                ))
            })?;
            let result = policy.enforce_verifier(&authority);
            policy.close().map_err(|_| {
                DeliveryTerminalOutcomeCommitError::Storage(StorageError::adapter(
                    "Verifier enterprise Policy authority could not close",
                ))
            })?;
            result.map_err(|error| {
                let message = match error.kind() {
                    WorkerPolicyErrorKind::Rejected => {
                        "Verifier enterprise Policy denied the terminal outcome"
                    }
                    WorkerPolicyErrorKind::Unavailable => {
                        "Verifier enterprise Policy authority is unavailable"
                    }
                };
                DeliveryTerminalOutcomeCommitError::Storage(StorageError::adapter(message))
            })?;
        }
        let commit = {
            let storage = self
                .storage_mut()
                .map_err(DeliveryTerminalOutcomeCommitError::Storage)?;
            terminal_outcome_transaction::execute_at(storage, scope, message, facts, server_time)?
        };
        let data_directory = self
            .local_enterprise_quota_directory()
            .map_err(WorkerExecutionLifecycleError::from);
        let worker_terminal = data_directory.and_then(|data_directory| {
            let lifecycle = DurableWorkerExecutionLifecycle::open(data_directory)?;
            match facts.status() {
                winwincode_delivery::application::stage::TerminalOutcomeStatus::Succeeded => {
                    lifecycle.settle_terminal_outcome(message).map(|_| ())
                }
                winwincode_delivery::application::stage::TerminalOutcomeStatus::Failed
                | winwincode_delivery::application::stage::TerminalOutcomeStatus::Cancelled
                | winwincode_delivery::application::stage::TerminalOutcomeStatus::InfrastructureError => {
                    lifecycle.release_terminal_outcome(message).map(|_| ())
                }
            }
        });
        if let Err(source) = worker_terminal {
            return Err(DeliveryTerminalOutcomeCommitError::WorkerQuotaPending {
                commit: Box::new(commit),
                source,
            });
        }
        if facts.status()
            != winwincode_delivery::application::stage::TerminalOutcomeStatus::Succeeded
            && let Err(source) = self.release_terminal_artifact_quota(message, facts.status())
        {
            return Err(DeliveryTerminalOutcomeCommitError::ArtifactQuotaPending {
                commit: Box::new(commit),
                source,
            });
        }
        self.flush_outbox().map_err(|source| {
            DeliveryTerminalOutcomeCommitError::PublicationPending {
                commit: Box::new(commit.clone()),
                source,
            }
        })?;
        Ok(commit)
    }

    /// Atomically commits a constructor-derived bounded-rework clarification
    /// without creating or dispatching an `ExecutionJob`.
    ///
    /// # Errors
    ///
    /// Returns a storage error before any durable write when the command,
    /// transition, revision, receipt, or journal publication is not exact.
    pub fn commit_delivery_rework_clarification(
        &mut self,
        command: &CommandEnvelope,
        transition: &winwincode_delivery::application::stage::StageAdvanceResult,
    ) -> Result<CommitReceipt, CommitError> {
        let receipt = {
            let storage = self.storage_mut().map_err(CommitError::Storage)?;
            rework_transaction::execute(storage, command, transition)
                .map_err(CommitError::Storage)?
        };
        self.flush_outbox()
            .map_err(|source| CommitError::PublicationPending {
                receipt: Box::new(receipt.clone()),
                source,
            })?;
        Ok(receipt)
    }

    /// Atomically promotes the exact ordered task graph sealed by the current
    /// approved solution review, then publishes only its committed outbox row.
    ///
    /// # Errors
    ///
    /// Returns before persistence for a stale review, changed revision, or any
    /// failed atomic member. Publication failure retains the committed event
    /// for replay.
    pub fn commit_delivery_task_breakdown(
        &mut self,
        command: &CommandEnvelope,
    ) -> Result<CommitReceipt, CommitError> {
        let receipt = {
            let storage = self.storage_mut().map_err(CommitError::Storage)?;
            task_breakdown_transaction::execute(storage, command).map_err(CommitError::Storage)?
        };
        self.flush_outbox()
            .map_err(|source| CommitError::PublicationPending {
                receipt: Box::new(receipt.clone()),
                source,
            })?;
        Ok(receipt)
    }

    /// Recomputes and atomically commits one Delivery's Evidence, Verdict,
    /// blocking Attention, task state, status, scoped receipt, journal record,
    /// and immutable outbox event.
    ///
    /// # Errors
    ///
    /// Returns before persistence for stale authoritative facts or when any
    /// atomic member fails. Publication failure carries the committed receipt
    /// and leaves the one event pending for replay.
    pub fn commit_delivery_verdict(
        &mut self,
        command: &CommandEnvelope,
        facts: SubmitVerdictFacts<'_>,
    ) -> Result<CommitReceipt, DeliveryVerdictCommitError> {
        let receipt = {
            let storage = self
                .storage_mut()
                .map_err(DeliveryVerdictCommitError::Storage)?;
            verdict_transaction::execute(storage, command, facts)?
        };
        self.flush_outbox()
            .map_err(|source| DeliveryVerdictCommitError::PublicationPending {
                receipt: Box::new(receipt.clone()),
                source,
            })?;
        Ok(receipt)
    }

    /// Loads canonical state through the configured storage adapter.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError`] when the adapter read fails.
    pub fn load_state(&self, stream_id: &str) -> Result<Option<StoredState>, StorageError> {
        self.storage_ref()?.load_state(stream_id)
    }

    /// Stops accepting work by consuming the host, flushes the outbox, closes
    /// the publisher, and finally closes storage.
    ///
    /// # Errors
    ///
    /// Returns [`ShutdownError`] after attempting every close step when outbox
    /// flush or adapter close fails.
    pub fn shutdown(mut self) -> Result<ShutdownReport, ShutdownError> {
        let mut failures = Vec::new();
        let published_event_count = match self.flush_outbox() {
            Ok(count) => count,
            Err(error) => {
                failures.push(format!("outbox flush failed: {error}"));
                0
            }
        };
        failures.extend(self.close_resources());
        if failures.is_empty() {
            Ok(ShutdownReport {
                published_event_count,
            })
        } else {
            Err(ShutdownError { failures })
        }
    }

    fn flush_outbox(&mut self) -> Result<usize, OutboxError> {
        self.flush_pending_audit_events()?;
        let events = self
            .storage_ref()
            .map_err(OutboxError::Acknowledge)?
            .pending_events()
            .map_err(OutboxError::Acknowledge)?;
        let mut published = 0;
        for event in events {
            self.publisher_mut()
                .map_err(|error| OutboxError::Publish(EventPublishError::new(error.to_string())))?
                .publish(&event)
                .map_err(OutboxError::Publish)?;
            self.storage_mut()
                .map_err(OutboxError::Acknowledge)?
                .mark_published(&event.event_id)
                .map_err(OutboxError::Acknowledge)?;
            published += 1;
        }
        Ok(published)
    }

    /// Flushes the durable audit bridge into the one immutable audit hash
    /// chain. The payload is a complete canonical `AuditEvent`; the `SQLite`
    /// row is only a pending marker and remains for idempotent crash recovery.
    fn flush_pending_audit_events(&mut self) -> Result<(), OutboxError> {
        let pending = self
            .storage_ref()
            .map_err(OutboxError::Acknowledge)?
            .pending_audit_events()
            .map_err(OutboxError::Acknowledge)?;
        for pending_event in pending {
            let event: AuditEvent =
                serde_json::from_slice(pending_event.payload()).map_err(|_| {
                    OutboxError::Acknowledge(StorageError::adapter(
                        "pending audit event payload is not canonical JSON",
                    ))
                })?;
            let canonical_payload = serde_json::to_vec(&event).map_err(|_| {
                OutboxError::Acknowledge(StorageError::adapter(
                    "pending audit event cannot be canonically encoded",
                ))
            })?;
            if canonical_payload != pending_event.payload()
                || event.event_id().as_str() != pending_event.event_id()
            {
                return Err(OutboxError::Acknowledge(StorageError::invalid_input(
                    "pending audit event does not match its canonical event identity",
                )));
            }
            self.audit_store
                .as_mut()
                .ok_or_else(|| OutboxError::Audit(winwincode_audit::AuditError::unavailable()))?
                .append(&event)
                .map_err(OutboxError::Audit)?;
            self.storage_mut()
                .map_err(OutboxError::Acknowledge)?
                .mark_audit_event_persisted(pending_event.event_id())
                .map_err(OutboxError::Acknowledge)?;
        }
        Ok(())
    }

    fn storage_ref(&self) -> Result<&dyn ProductStateStorage, StorageError> {
        self.storage
            .as_deref()
            .ok_or_else(|| StorageError::adapter("Control Plane storage is closed"))
    }

    fn storage_mut(&mut self) -> Result<&mut (dyn ProductStateStorage + 'static), StorageError> {
        self.storage
            .as_deref_mut()
            .ok_or_else(|| StorageError::adapter("Control Plane storage is closed"))
    }

    fn publisher_mut(&mut self) -> Result<&mut (dyn EventPublisher + 'static), StartError> {
        self.publisher
            .as_deref_mut()
            .ok_or_else(|| StartError::new("Control Plane event publisher is closed"))
    }

    fn close_resources(&mut self) -> Vec<String> {
        let mut failures = Vec::new();
        self.publication_providers.take();
        self.publication_authority.take();
        if let Some(mut publisher) = self.publisher.take()
            && let Err(error) = publisher.close()
        {
            failures.push(format!("event publisher close failed: {error}"));
        }
        if let Some(storage) = self.storage.take()
            && let Err(error) = storage.close()
        {
            failures.push(format!("storage close failed: {error}"));
        }
        if let Some(audit_store) = self.audit_store.take()
            && let Err(error) = audit_store.close()
        {
            failures.push(format!("audit store close failed: {error}"));
        }
        if let Some(artifact_store) = self.artifact_store.take()
            && let Err(error) = artifact_store.close()
        {
            failures.push(format!("Artifact store close failed: {error}"));
        }
        if let Some(temporary_root) = self.temporary_root.take()
            && let Err(error) = temporary_root.release()
        {
            failures.push(format!("temporary root release failed: {error}"));
        }
        failures
    }
}

fn reserved_public_projection_topic(topic: &str) -> bool {
    serde_json::from_value::<ControlPlaneWebSocketEventType>(serde_json::Value::String(
        topic.to_owned(),
    ))
    .is_ok()
}

fn reserved_delivery_transaction_topic(topic: &str) -> bool {
    topic.starts_with("delivery.") || topic.starts_with("runtime.")
}

fn authority_lease(
    authority: &winwincode_delivery::application::stage::SessionBindingAuthority,
) -> execution_port::ExecutionLeaseStamp {
    let active = authority.active_lease();
    execution_port::ExecutionLeaseStamp {
        attempt: i64::try_from(active.attempt()).unwrap_or(i64::MAX),
        expires_at: authority.expires_at().clone(),
        fencing_token: active.fencing_token().clone(),
        issued_at: authority.issued_at().clone(),
        job_id: active.execution_job_id().clone(),
        lease_id: active.lease_id().clone(),
        worker_id: active.worker_id().clone(),
        worker_instance_id: active.worker_instance_id().clone(),
    }
}

fn artifact_ack_digest(
    acknowledgement: &execution_port::ArtifactAckMessage,
) -> Result<Sha256Digest, CandidateResolutionError> {
    // A replay of the same final chunk is acknowledged as `Duplicate` while
    // the original write was acknowledged as `Accepted`.  Both responses
    // describe the same immutable final write, so retention identity must use
    // one canonical status rather than creating a second pin on replay.
    let mut canonical = acknowledgement.clone();
    canonical.status = execution_port::LeaseWriteStatus::Accepted;
    let bytes = serde_json::to_vec(&canonical).map_err(|_| {
        CandidateResolutionError::Storage(StorageError::adapter(
            "final Artifact acknowledgement cannot be encoded",
        ))
    })?;
    Ok(Sha256Digest(format!("sha256:{:x}", Sha256::digest(bytes))))
}

pub(crate) fn storage_commit(
    command: &CommandEnvelope,
    change: StateChange,
) -> Result<StateCommit, StorageError> {
    let (receipt_identity, command_digest) = command_receipt(command)?;
    let expected_revision = u64::try_from(command.expected_revision.0).map_err(|_| {
        StorageError::invalid_input("command expectedRevision must not be negative")
    })?;

    Ok(StateCommit::new(
        receipt_identity,
        command_digest,
        change.stream_id,
        expected_revision,
        change.state,
        change.events,
    ))
}

const EXECUTION_AUDIT_SYSTEM_ACTOR: &str = "sys_00000000000000000000000000";
const EXECUTION_AUDIT_ORIGIN: &str = "control-plane.execution-port";

/// Encodes one complete execution audit event for the durable storage bridge.
///
/// The storage crate receives only the canonical event bytes and its event id;
/// all event semantics remain owned by `winwincode-audit`, and the immutable
/// `AuditStore` is the sole audit authority after the bridge is flushed.
#[allow(clippy::too_many_arguments)]
pub(crate) fn execution_audit_event(
    event_id: AuditEventId,
    occurred_at_millis: u64,
    request_id: RequestId,
    scope: &RepositoryScope,
    action: AuditAction,
    before: &Delivery,
    after: &Delivery,
    subject: AuditSubject,
    result_code: &str,
) -> Result<PendingAuditEvent, StorageError> {
    let before = delivery_state_digest(before)?;
    let after = delivery_state_digest(after)?;
    let state = AuditState::changed(Some(before), after).map_err(|error| {
        StorageError::invalid_input(format!("execution audit state is invalid: {error}"))
    })?;
    execution_audit_event_with_state(
        event_id,
        occurred_at_millis,
        request_id,
        scope,
        action,
        state,
        subject,
        result_code,
    )
}

/// Encodes one execution audit event from a state transition owned by the
/// caller. Runtime ledger transitions use this seam because they do not mutate
/// the Delivery aggregate.
#[allow(clippy::too_many_arguments)]
pub(crate) fn execution_audit_event_with_state(
    event_id: AuditEventId,
    occurred_at_millis: u64,
    request_id: RequestId,
    scope: &RepositoryScope,
    action: AuditAction,
    state: AuditState,
    subject: AuditSubject,
    result_code: &str,
) -> Result<PendingAuditEvent, StorageError> {
    let scope = AuditScope::repository(
        scope.organization_id.clone(),
        scope.workspace_id.clone(),
        scope.project_id.clone(),
        scope.repository_id.clone(),
    )
    .map_err(|error| {
        StorageError::invalid_input(format!("execution audit scope is invalid: {error}"))
    })?;
    let origin = AuditOrigin::local(EXECUTION_AUDIT_ORIGIN).map_err(|error| {
        StorageError::invalid_input(format!("execution audit origin is invalid: {error}"))
    })?;
    let event = AuditEvent::state_change(
        event_id,
        occurred_at_millis,
        AuditActor::System(SystemActorId(EXECUTION_AUDIT_SYSTEM_ACTOR.to_owned())),
        scope,
        request_id,
        action,
        state,
        origin,
        subject,
        result_code,
        AuditRetention::Indefinite,
    )
    .map_err(|error| {
        StorageError::invalid_input(format!("execution audit event is invalid: {error}"))
    })?;
    let event_id = event.event_id().as_str().to_owned();
    let payload = serde_json::to_vec(&event).map_err(|error| {
        StorageError::adapter(format!("failed to encode execution audit event: {error}"))
    })?;
    PendingAuditEvent::new(event_id, payload)
}

fn delivery_state_digest(delivery: &Delivery) -> Result<Sha256Digest, StorageError> {
    let payload = delivery.encode_json().map_err(|error| {
        StorageError::adapter(format!("failed to encode Delivery for audit: {error}"))
    })?;
    Ok(Sha256Digest(format!(
        "sha256:{:x}",
        Sha256::digest(payload)
    )))
}

pub(crate) fn public_event_actor(actor: &Actor) -> PublicEventActor {
    match actor {
        Actor::UserActor(actor) => PublicEventActor::User {
            id: actor.id.clone(),
        },
        Actor::ServiceAccountActor(actor) => PublicEventActor::ServiceAccount {
            id: actor.id.clone(),
        },
        Actor::SystemActor(actor) => PublicEventActor::System {
            id: actor.id.clone(),
        },
    }
}

pub(crate) fn public_event_scope(scope: &Scope) -> PublicEventScope {
    match scope {
        Scope::OrganizationScope(scope) => PublicEventScope::Organization {
            organization_id: scope.organization_id.clone(),
        },
        Scope::WorkspaceScope(scope) => PublicEventScope::Workspace {
            organization_id: scope.organization_id.clone(),
            workspace_id: scope.workspace_id.clone(),
        },
        Scope::ProjectScope(scope) => PublicEventScope::Project {
            organization_id: scope.organization_id.clone(),
            workspace_id: scope.workspace_id.clone(),
            project_id: scope.project_id.clone(),
        },
        Scope::RepositoryScope(scope) => public_repository_scope(scope),
    }
}

pub(crate) fn public_repository_scope(scope: &RepositoryScope) -> PublicEventScope {
    PublicEventScope::Repository {
        organization_id: scope.organization_id.clone(),
        workspace_id: scope.workspace_id.clone(),
        project_id: scope.project_id.clone(),
        repository_id: scope.repository_id.clone(),
    }
}

fn receipt_actor_key(actor: &Actor) -> Result<ReceiptActorKey, StorageError> {
    storage_receipt_actor_key(&public_event_actor(actor))
}

fn receipt_scope_key(scope: &Scope) -> Result<ReceiptScopeKey, StorageError> {
    storage_receipt_scope_key(&public_event_scope(scope))
}

/// Builds the one canonical storage receipt identity from a generated command
/// envelope's authenticated actor, exact tenant scope, and request id.
///
/// # Errors
///
/// Rejects a non-canonical actor, scope, or request identity.
pub fn command_receipt_identity(
    actor: &Actor,
    scope: &Scope,
    request_id: RequestId,
) -> Result<ReceiptIdentity, StorageError> {
    storage_public_receipt_identity(
        &public_event_actor(actor),
        &public_event_scope(scope),
        request_id,
    )
}

pub(crate) fn repository_scope_key(
    scope: &RepositoryScope,
) -> Result<ReceiptScopeKey, StorageError> {
    storage_receipt_scope_key(&public_repository_scope(scope))
}

pub(crate) fn repository_scope_from_receipt_key(
    key: &ReceiptScopeKey,
) -> Result<RepositoryScope, StorageError> {
    let PublicEventScope::Repository {
        organization_id,
        workspace_id,
        project_id,
        repository_id,
    } = storage_repository_scope_from_receipt_key(key)?
    else {
        return Err(StorageError::invalid_input(
            "receipt scope is not a repository",
        ));
    };
    Ok(RepositoryScope {
        kind: winwincode_api::generated::RepositoryScopeKind::Repository,
        organization_id,
        workspace_id,
        project_id,
        repository_id,
    })
}

pub(crate) fn delivery_changed_event(
    command: &CommandEnvelope,
    delivery_id: &DeliveryId,
    delivery_revision: u64,
    change_kind: DeliveryChangeKind,
    occurred_at: Instant,
    component: &'static str,
) -> Result<NewOutboxEvent, StorageError> {
    let Scope::RepositoryScope(scope) = &command.scope else {
        return Err(StorageError::invalid_input(
            "Delivery change events require repository scope",
        ));
    };
    delivery_changed_event_for_scope(
        public_repository_scope(scope),
        delivery_id,
        delivery_revision,
        change_kind,
        occurred_at,
        PublicEventSource::ControlPlane {
            actor: public_event_actor(&command.actor),
            component: component.to_owned(),
        },
    )
}

pub(crate) fn delivery_changed_event_for_scope(
    scope: PublicEventScope,
    delivery_id: &DeliveryId,
    delivery_revision: u64,
    change_kind: DeliveryChangeKind,
    occurred_at: Instant,
    source: PublicEventSource,
) -> Result<NewOutboxEvent, StorageError> {
    let revision = i64::try_from(delivery_revision)
        .map(Revision)
        .map_err(|_| StorageError::invalid_input("Delivery revision exceeds the public range"))?;
    let payload = ControlPlaneWebSocketDeliveryChangedEvent {
        change_kind: change_kind.as_str().to_owned(),
        delivery_id: delivery_id.clone(),
        revision,
        type_value: ControlPlaneWebSocketDeliveryChangedEventTypeValue::DeliveryChangedV1,
    };
    let payload = serde_json::to_vec(&payload).map_err(|error| {
        StorageError::adapter(format!("failed to encode Delivery change event: {error}"))
    })?;
    let topic = delivery_changed_topic()?;
    let scope_key = storage_receipt_scope_key(&scope)?;
    let event_id = delivery_changed_event_id(&scope_key, &payload);
    NewOutboxEvent::public_projection(
        event_id,
        topic,
        payload,
        ProjectionEventStream::Delivery(delivery_id.clone()),
        scope,
        occurred_at,
        source,
    )
}

pub(crate) fn instant_from_millis(value: u64) -> Result<Instant, StorageError> {
    let seconds = value / 1_000;
    let millis = value % 1_000;
    let days = i64::try_from(seconds / 86_400)
        .map_err(|_| StorageError::invalid_input("timestamp exceeds RFC 3339"))?;
    let seconds_of_day = seconds % 86_400;
    let (year, month, day) = civil_from_days(days);
    if !(1970..=9999).contains(&year) {
        return Err(StorageError::invalid_input("timestamp exceeds RFC 3339"));
    }
    let hour = seconds_of_day / 3_600;
    let minute = (seconds_of_day % 3_600) / 60;
    let second = seconds_of_day % 60;
    Ok(Instant(format!(
        "{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{millis:03}Z"
    )))
}

const fn civil_from_days(days_since_epoch: i64) -> (i64, i64, i64) {
    let z = days_since_epoch + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let mut year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    year += (month <= 2) as i64;
    (year, month, day)
}

pub(crate) fn validate_delivery_changed_receipt(
    receipt: &CommitReceipt,
    delivery_id: &DeliveryId,
    delivery_revision: u64,
    change_kind: DeliveryChangeKind,
) -> Result<(), StorageError> {
    let topic = delivery_changed_topic()?;
    let matching = receipt
        .events
        .iter()
        .filter(|event| event.topic == topic)
        .collect::<Vec<_>>();
    let [event] = matching.as_slice() else {
        return Err(StorageError::invalid_input(
            "durable receipt must contain exactly one Delivery change event",
        ));
    };
    let payload: ControlPlaneWebSocketDeliveryChangedEvent = serde_json::from_slice(&event.payload)
        .map_err(|_| {
            StorageError::invalid_input("durable Delivery change event is not canonical")
        })?;
    if serde_json::to_vec(&payload).map_err(|_| {
        StorageError::invalid_input("durable Delivery change event is not canonical")
    })? != event.payload
        || payload.type_value
            != ControlPlaneWebSocketDeliveryChangedEventTypeValue::DeliveryChangedV1
        || payload.delivery_id != *delivery_id
        || payload.revision.0 != i64::try_from(delivery_revision).unwrap_or(-1)
        || payload.change_kind != change_kind.as_str()
    {
        return Err(StorageError::invalid_input(
            "durable Delivery change event does not match committed Delivery facts",
        ));
    }
    let cursor = event.projection_cursor.as_ref().ok_or_else(|| {
        StorageError::invalid_input("durable Delivery change event has no stream cursor")
    })?;
    if cursor.sequence() == 0
        || cursor.event_id().map(|value| value.0.as_str()) != Some(event.event_id.as_str())
        || cursor.key().scope_key() != receipt.receipt_identity.scope_key()
        || cursor.key().stream() != &ProjectionEventStream::Delivery(delivery_id.clone())
        || event.event_id
            != delivery_changed_event_id(receipt.receipt_identity.scope_key(), &event.payload).0
    {
        return Err(StorageError::invalid_input(
            "durable Delivery change event cursor is not exact",
        ));
    }
    Ok(())
}

fn delivery_changed_topic() -> Result<String, StorageError> {
    match serde_json::to_value(
        ControlPlaneWebSocketDeliveryChangedEventTypeValue::DeliveryChangedV1,
    )
    .map_err(|error| {
        StorageError::adapter(format!(
            "failed to encode the generated Delivery change event type: {error}"
        ))
    })? {
        serde_json::Value::String(topic) => Ok(topic),
        _ => Err(StorageError::adapter(
            "generated Delivery change event type did not encode as a string",
        )),
    }
}

fn delivery_changed_event_id(scope_key: &ReceiptScopeKey, payload: &[u8]) -> ControlPlaneEventId {
    let mut digest = Sha256::new();
    digest.update(b"winwincode.delivery-changed-event.v1\0");
    digest.update((scope_key.as_bytes().len() as u64).to_be_bytes());
    digest.update(scope_key.as_bytes());
    digest.update((payload.len() as u64).to_be_bytes());
    digest.update(payload);
    ControlPlaneEventId(format!("evt_{:x}", digest.finalize()))
}

pub(crate) fn command_receipt(
    command: &CommandEnvelope,
) -> Result<(ReceiptIdentity, Sha256Digest), StorageError> {
    let actor_key = receipt_actor_key(&command.actor)?;
    let scope_key = receipt_scope_key(&command.scope)?;
    require_canonical_id(&command.request_id.0, "req_", "command requestId")?;
    let receipt_identity = ReceiptIdentity::new(actor_key, scope_key, command.request_id.clone())?;
    let serialized = serde_json::to_vec(command).map_err(|error| {
        StorageError::adapter(format!(
            "failed to encode the canonical command digest: {error}"
        ))
    })?;
    let digest = Sha256::digest(serialized);
    let command_digest = Sha256Digest(format!("sha256:{digest:x}"));
    Ok((receipt_identity, command_digest))
}

fn delivery_command(command: &CommandName) -> bool {
    matches!(
        command,
        CommandName::DeliveryCreate
            | CommandName::DeliveryUpdateSpec
            | CommandName::DeliveryApproveTaskBreakdown
            | CommandName::DeliveryAdvance
            | CommandName::DeliveryResolveAttention
            | CommandName::DeliverySubmitVerdict
    )
}

fn require_canonical_id(value: &str, prefix: &str, label: &str) -> Result<(), StorageError> {
    let Some(suffix) = value.strip_prefix(prefix) else {
        return Err(StorageError::invalid_input(format!(
            "{label} is not canonical"
        )));
    };
    if suffix.len() != 26 || !suffix.bytes().all(is_crockford_base32) {
        return Err(StorageError::invalid_input(format!(
            "{label} is not canonical"
        )));
    }
    Ok(())
}

const fn is_crockford_base32(byte: u8) -> bool {
    matches!(
        byte,
        b'0'..=b'9'
            | b'A'..=b'H'
            | b'J'
            | b'K'
            | b'M'
            | b'N'
            | b'P'..=b'T'
            | b'V'..=b'Z'
    )
}

fn close_publisher(publisher: &mut Box<dyn EventPublisher>) -> String {
    publisher.close().err().map_or_else(String::new, |error| {
        format!("; event publisher close also failed: {error}")
    })
}

fn cleanup_suffix(failures: &[String]) -> String {
    if failures.is_empty() {
        String::new()
    } else {
        format!("; cleanup also failed: {}", failures.join("; "))
    }
}
