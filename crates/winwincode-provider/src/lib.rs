// SPDX-License-Identifier: Apache-2.0

//! Provider IO and stream conversion for the device execution runtime.
//! This crate does not depend on the Server or Control Plane.

pub mod credential_leak_gate;
mod jev;
mod provider_anthropic;
pub mod provider_https_sse;
pub mod provider_stream;
mod types;

pub use credential_leak_gate::{
    CredentialLeakError, CredentialLeakErrorKind, CredentialLeakGate, CredentialOutputBoundary,
};
pub use jev::{
    HttpsJevRemoteTransport, JevAttemptFailure, JevBatchEvaluation, JevDevice, JevDtype,
    JevEvaluation, JevExecutionOptions, JevFallbackPolicy, JevHealth, JevHypothesis,
    JevObservation, JevProvider, JevProviderCapabilities, JevProviderError, JevProviderErrorKind,
    JevRemoteScoreItem, JevRemoteScoreRequest, JevRemoteTransport, JevRun, JevRuntime,
    JevRuntimeConfig, JevScores, MockJevProvider, MockJevRemoteTransport,
    OPENJEV_DEFAULT_MODEL_ID, OPENJEV_LOCAL_RUNTIME_GAP, OpenJevLocalProvider, OpenJevLocalRuntime,
    OpenJevRemoteConfig, OpenJevRemoteConfigRequest, OpenJevRemoteProvider, OpenJevRemoteSettings,
    RemoteJevScoreBatch, contract_capability_mocks, jev_provider_order, parse_jev_score_list,
    parse_jev_scores, portable_scores,
};
pub use provider_anthropic::ProviderTokenPricing;
pub use provider_https_sse::{
    HttpsSseProviderAdapter, HttpsSseProviderCompletion, HttpsSseProviderConfig,
    HttpsSseProviderError, HttpsSseProviderErrorKind, HttpsSseProviderLimits,
    HttpsSseProviderTimeouts, MAX_ENDPOINT_BYTES, ProviderTlsRoots, canonical_https_endpoint,
};
pub use provider_stream::{
    CanonicalModelStreamFrame, ProviderFinishReason, ProviderStreamConversionError,
    ProviderStreamConversionErrorKind, ProviderStreamConverter, ProviderStreamEvent,
    ProviderStreamFailure, ProviderStreamFailureKind, ProviderTokenUsage, ProviderToolIdentity,
    ProviderToolIdentityError, ProviderToolKind,
};
pub use types::{
    ModelAttemptCharge, ModelAttemptFailureFact, ModelAttemptFailureKind, ModelExecutionCertainty,
    ProviderAdapterError, ProviderAdapterErrorKind, ProviderAdapterInvocation,
    ProviderAdapterOpenReceipt, ProviderAdapterPort, ProviderGatewayErrorKind,
    ProviderGatewayOpenReceipt, ProviderGatewayTerminal, ProviderGatewayTerminalCharge,
    ProviderGatewayTerminalOutcome, ProviderStreamControlAction, ResolvedSecret, SecretStoreError,
    SecretStoreErrorKind,
};

mod device_store;
pub use device_store::{DeviceProviderError, DeviceProviderStore, valid_device_provider_config};

mod device_model;
pub use device_model::{model_failure, public_model_chunk};

mod device_extensions;
mod mcp_connection;
pub use device_extensions::InstalledMcpTools;
