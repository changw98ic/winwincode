// SPDX-License-Identifier: Apache-2.0

//! Worker-side MCP and migrated plugin capability boundary.
//!
//! Discovery is declarative: it records the MCP target exposed by embedded
//! Codex Core, its version, health, and provenance. This module does not load a
//! plugin or implement an MCP runtime. Every authorized invocation is forwarded
//! to the canonical [`WorkerActionGateway`], so the existing deterministic Gate,
//! trace recorder, and Codex Core executor remain the only side-effect path.

use std::fmt;

use serde::Serialize;
use sha2::{Digest, Sha256};
use winwincode_domain::{Instant, Sha256Digest, WorkerSessionId};

use crate::action_enforcement::{ActionEnforcementVerifier, ActionReceiptUseStore};
use crate::action_gateway::{
    ActionGatewayError, CodexToolExecutor, DeterministicActionGate, ExecutedAction,
    ExecutionEnvelopeToken, PreActionDecisionRecorder, WorkerActionGateway, WorkerActionRequest,
};
use crate::action_normalizer::{ToolRequest, canonical_mcp_capability_id};
use crate::generated::{
    ActionEnforcementReceiptMessage, WorkerCapabilityFeature, WorkerCapabilitySet,
};

/// Version of the canonical Worker capability discovery contract.
pub const CAPABILITY_ADAPTER_VERSION: &str = "winwincode-worker-capability/v1";

/// Declarative origin of a discovered capability.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityOrigin {
    /// MCP tool discovered directly from embedded Codex Core.
    CodexCoreMcp,
    /// A retained plugin feature mapped to an embedded Codex Core MCP tool.
    ///
    /// This is manifest provenance only; no plugin runtime is loaded here.
    MappedPluginManifest,
    /// A discovered capability whose side effects cannot be fully governed.
    Unmanaged,
}

/// Current health reported for one discovered capability version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityHealth {
    /// Capability is ready for invocation.
    Healthy,
    /// Capability is usable, but the caller must receive a warning.
    Degraded,
    /// Capability must fail closed before reaching Codex Core.
    Unavailable,
}

/// Policy applied to a capability explicitly marked unmanaged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnmanagedCapabilityPolicy {
    /// Run through the normal Action Gateway and return a visible warning.
    Warn,
    /// Reject before any Gate or tool side effect.
    Deny,
}

/// Stable catalog validation category.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapabilityCatalogErrorCode {
    /// Worker Registry capability digest is malformed.
    InvalidRegistryCapabilityDigest,
    /// The Worker Registry profile does not advertise MCP support.
    McpFeatureNotRegistered,
    /// Capability version is blank or not a portable identifier.
    InvalidCapabilityVersion,
    /// Two entries claim the same canonical MCP target.
    DuplicateCapability,
    /// Deterministic discovery fingerprinting failed.
    CatalogDigestFailure,
    /// Requested capability is absent from the discovered catalog.
    UnknownCapability,
}

/// Secret-free catalog construction failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapabilityCatalogError {
    /// Stable machine-readable category.
    pub code: CapabilityCatalogErrorCode,
    /// Secret-free explanation.
    pub reason: String,
}

impl fmt::Display for CapabilityCatalogError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.reason)
    }
}

impl std::error::Error for CapabilityCatalogError {}

/// One versioned MCP target advertised by the Worker.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub struct CapabilityDescriptor {
    id: String,
    version: String,
    health: CapabilityHealth,
    origin: CapabilityOrigin,
}

impl CapabilityDescriptor {
    /// Builds a descriptor from secret-free MCP server and tool identifiers.
    ///
    /// # Errors
    ///
    /// Returns a catalog validation error for an invalid capability version, or
    /// the normalizer's invalid-identifier error for an invalid MCP target.
    pub fn mcp(
        server: &str,
        tool: &str,
        version: &str,
        health: CapabilityHealth,
        origin: CapabilityOrigin,
    ) -> Result<Self, CapabilityDescriptorError> {
        validate_version(version).map_err(CapabilityDescriptorError::Catalog)?;
        let id = canonical_mcp_capability_id(server, tool)
            .map_err(|_| CapabilityDescriptorError::InvalidMcpTarget)?;
        Ok(Self {
            id,
            version: version.to_owned(),
            health,
            origin,
        })
    }

    /// Canonical `mcp://server/tool` target used by Action Normalization.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Exact discovered capability version.
    #[must_use]
    pub fn version(&self) -> &str {
        &self.version
    }

    /// Current discovered health.
    #[must_use]
    pub const fn health(&self) -> CapabilityHealth {
        self.health
    }

    /// Declarative discovery provenance.
    #[must_use]
    pub const fn origin(&self) -> CapabilityOrigin {
        self.origin
    }
}

/// Descriptor construction failure without echoing input values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CapabilityDescriptorError {
    /// MCP server or tool name is invalid.
    InvalidMcpTarget,
    /// Version is invalid.
    Catalog(CapabilityCatalogError),
}

impl fmt::Display for CapabilityDescriptorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidMcpTarget => formatter.write_str("MCP capability target is invalid"),
            Self::Catalog(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for CapabilityDescriptorError {}

/// Deterministic Worker discovery snapshot bound to its Registry capability set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerCapabilityCatalog {
    registry_capability_digest: Sha256Digest,
    catalog_digest: Sha256Digest,
    capabilities: Vec<CapabilityDescriptor>,
}

impl WorkerCapabilityCatalog {
    /// Validates, sorts, and fingerprints one Worker capability discovery result.
    ///
    /// # Errors
    ///
    /// Returns a fail-closed catalog error for a malformed Registry digest,
    /// missing MCP feature, invalid version, or duplicate canonical MCP target.
    pub fn discover(
        registered: &WorkerCapabilitySet,
        mut capabilities: Vec<CapabilityDescriptor>,
    ) -> Result<Self, CapabilityCatalogError> {
        if !is_sha256_digest(&registered.capability_digest.0) {
            return Err(catalog_error(
                CapabilityCatalogErrorCode::InvalidRegistryCapabilityDigest,
                "Worker Registry capability digest is invalid",
            ));
        }
        if !capabilities.is_empty() && !registered.features.contains(&WorkerCapabilityFeature::Mcp)
        {
            return Err(catalog_error(
                CapabilityCatalogErrorCode::McpFeatureNotRegistered,
                "Worker Registry profile does not advertise MCP capability",
            ));
        }
        for capability in &capabilities {
            validate_version(&capability.version)?;
        }
        capabilities.sort();
        if capabilities.windows(2).any(|pair| pair[0].id == pair[1].id) {
            return Err(catalog_error(
                CapabilityCatalogErrorCode::DuplicateCapability,
                "capability discovery contains a duplicate MCP target",
            ));
        }

        let digest_input = serde_json::to_vec(&(
            CAPABILITY_ADAPTER_VERSION,
            &registered.capability_digest,
            &capabilities,
        ))
        .map_err(|_| {
            catalog_error(
                CapabilityCatalogErrorCode::CatalogDigestFailure,
                "capability discovery fingerprint could not be computed",
            )
        })?;
        let catalog_digest = Sha256Digest(format!("sha256:{:x}", Sha256::digest(digest_input)));
        Ok(Self {
            registry_capability_digest: registered.capability_digest.clone(),
            catalog_digest,
            capabilities,
        })
    }

    /// Exact adapter contract version used for this snapshot.
    #[must_use]
    pub const fn adapter_version(&self) -> &'static str {
        CAPABILITY_ADAPTER_VERSION
    }

    /// Worker Registry capability digest this discovery snapshot extends.
    #[must_use]
    pub const fn registry_capability_digest(&self) -> &Sha256Digest {
        &self.registry_capability_digest
    }

    /// Deterministic digest of the Registry profile and sorted descriptors.
    #[must_use]
    pub const fn catalog_digest(&self) -> &Sha256Digest {
        &self.catalog_digest
    }

    /// Sorted discovered capabilities. No endpoint, argument, credential, or
    /// plugin configuration is present in this observable snapshot.
    #[must_use]
    pub fn capabilities(&self) -> &[CapabilityDescriptor] {
        &self.capabilities
    }

    /// Issues an exact session-and-envelope grant for one discovered version.
    ///
    /// # Errors
    ///
    /// Returns `UnknownCapability` when `capability_id` is absent.
    pub fn authorize(
        &self,
        capability_id: &str,
        worker_session_id: WorkerSessionId,
        envelope: ExecutionEnvelopeToken,
    ) -> Result<CapabilityGrant, CapabilityCatalogError> {
        let descriptor = self
            .capabilities
            .iter()
            .find(|capability| capability.id == capability_id)
            .ok_or_else(|| {
                catalog_error(
                    CapabilityCatalogErrorCode::UnknownCapability,
                    "capability is absent from the Worker discovery snapshot",
                )
            })?;
        Ok(CapabilityGrant {
            capability_id: descriptor.id.clone(),
            capability_version: descriptor.version.clone(),
            catalog_digest: self.catalog_digest.clone(),
            worker_session_id,
            envelope,
        })
    }
}

/// Exact authority for one discovered capability version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapabilityGrant {
    capability_id: String,
    capability_version: String,
    catalog_digest: Sha256Digest,
    worker_session_id: WorkerSessionId,
    envelope: ExecutionEnvelopeToken,
}

impl CapabilityGrant {
    /// Canonical MCP capability target authorized by this grant.
    #[must_use]
    pub fn capability_id(&self) -> &str {
        &self.capability_id
    }

    /// Exact discovered version authorized by this grant.
    #[must_use]
    pub fn capability_version(&self) -> &str {
        &self.capability_version
    }

    /// `WorkerSession` to which this grant is confined.
    #[must_use]
    pub const fn worker_session_id(&self) -> &WorkerSessionId {
        &self.worker_session_id
    }

    /// Execution Envelope to which this grant is confined.
    #[must_use]
    pub const fn envelope(&self) -> &ExecutionEnvelopeToken {
        &self.envelope
    }
}

/// Explicit versioned capability claim accompanying one MCP action.
#[derive(Debug, Clone, Copy)]
pub struct CapabilityInvocationRequest<'action> {
    /// Canonical `mcp://server/tool` identifier claimed by the caller.
    pub capability_id: &'action str,
    /// Exact descriptor version claimed by the caller.
    pub capability_version: &'action str,
    /// Canonical Action Gateway request containing the actual MCP request.
    pub action: &'action WorkerActionRequest,
    /// Control Plane-issued permit for this exact action invocation.
    pub enforcement_receipt: &'action ActionEnforcementReceiptMessage,
}

/// Non-secret warning returned with an invocation that policy allowed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapabilityWarning {
    /// The capability is explicitly unmanaged.
    UnmanagedCapability,
    /// Discovery reported degraded health.
    DegradedCapability,
}

/// Successful capability invocation routed through the Action Gateway.
#[derive(Debug, Clone, PartialEq)]
pub struct CapabilityInvocation<Output> {
    /// Warning determined before invoking the normal Action Gateway.
    pub warnings: Vec<CapabilityWarning>,
    /// Existing gateway result proving Gate and Codex execution were used.
    pub executed: ExecutedAction<Output>,
}

/// Stable adapter rejection category.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapabilityRejectionCode {
    /// The Action Gateway request is not an MCP request.
    RequestFamilyMismatch,
    /// Caller claim does not match the actual normalized MCP target.
    CapabilityTargetMismatch,
    /// Capability is absent from the current discovery snapshot.
    UnknownCapability,
    /// Caller claim is for a different discovered version.
    CapabilityVersionMismatch,
    /// Capability is currently unavailable.
    CapabilityUnavailable,
    /// Current policy rejects explicitly unmanaged capabilities.
    UnmanagedCapabilityDenied,
    /// No grant exists for this capability.
    CapabilityNotAuthorized,
    /// Grant is bound to another `WorkerSession`.
    StaleWorkerSession,
    /// Grant is bound to another Execution Envelope.
    StaleExecutionEnvelope,
    /// Grant is bound to a replaced discovery snapshot or capability version.
    StaleCapabilityCatalog,
}

/// Capability adapter failure. Gateway errors retain their original typed cause.
#[derive(Debug, Clone, PartialEq)]
pub enum CapabilityAdapterError<RecorderError, ExecutorError> {
    /// Stable secret-free capability rejection before Gate execution.
    Rejected {
        /// Machine-readable rejection category.
        code: CapabilityRejectionCode,
        /// Secret-free explanation.
        reason: String,
    },
    /// The actual MCP target itself is invalid.
    InvalidMcpTarget,
    /// Request reached the canonical Action Gateway and failed there.
    Gateway(Box<ActionGatewayError<RecorderError, ExecutorError>>),
}

/// Result returned by the Worker Capability Adapter invocation point.
pub type CapabilityAdapterResult<Output, RecorderError, ExecutorError> =
    Result<CapabilityInvocation<Output>, CapabilityAdapterError<RecorderError, ExecutorError>>;

impl<RecorderError: fmt::Display, ExecutorError: fmt::Display> fmt::Display
    for CapabilityAdapterError<RecorderError, ExecutorError>
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Rejected { reason, .. } => formatter.write_str(reason),
            Self::InvalidMcpTarget => formatter.write_str("MCP capability target is invalid"),
            Self::Gateway(error) => {
                write!(formatter, "Action Gateway rejected capability: {error}")
            }
        }
    }
}

impl<RecorderError, ExecutorError> std::error::Error
    for CapabilityAdapterError<RecorderError, ExecutorError>
where
    RecorderError: std::error::Error + 'static,
    ExecutorError: std::error::Error + 'static,
{
}

/// Declarative capability authority layered over the single Action Gateway.
pub struct WorkerCapabilityAdapter<Policy, Gate, Recorder, Executor> {
    catalog: WorkerCapabilityCatalog,
    grants: Vec<CapabilityGrant>,
    unmanaged_policy: UnmanagedCapabilityPolicy,
    gateway: WorkerActionGateway<Policy, Gate, Recorder, Executor>,
}

impl<Policy, Gate, Recorder, Executor> WorkerCapabilityAdapter<Policy, Gate, Recorder, Executor>
where
    Gate: DeterministicActionGate<Policy>,
    Recorder: PreActionDecisionRecorder<Policy>,
    Executor: CodexToolExecutor,
{
    /// Creates an adapter around the existing canonical Action Gateway.
    #[must_use]
    pub fn new(
        catalog: WorkerCapabilityCatalog,
        grants: Vec<CapabilityGrant>,
        unmanaged_policy: UnmanagedCapabilityPolicy,
        gateway: WorkerActionGateway<Policy, Gate, Recorder, Executor>,
    ) -> Self {
        Self {
            catalog,
            grants,
            unmanaged_policy,
            gateway,
        }
    }

    /// Replaces discovery atomically. Existing grants become stale because they
    /// retain the old catalog digest and must be explicitly re-authorized.
    pub fn replace_catalog(&mut self, catalog: WorkerCapabilityCatalog) {
        self.catalog = catalog;
    }

    /// Replaces the complete current capability authorization set.
    pub fn replace_grants(&mut self, grants: Vec<CapabilityGrant>) {
        self.grants = grants;
    }

    /// Returns the current explicit discovery snapshot.
    #[must_use]
    pub const fn catalog(&self) -> &WorkerCapabilityCatalog {
        &self.catalog
    }

    /// Validates discovery, version, health, and session-scoped authorization,
    /// then invokes the existing Action Gateway.
    ///
    /// # Errors
    ///
    /// Returns a fail-closed capability rejection before Gate execution, or the
    /// original typed Action Gateway failure after routing the request there.
    pub fn invoke(
        &mut self,
        now: &Instant,
        invocation: CapabilityInvocationRequest<'_>,
        verifier: &ActionEnforcementVerifier,
        receipt_store: &mut dyn ActionReceiptUseStore,
    ) -> CapabilityAdapterResult<Executor::Output, Recorder::Error, Executor::Error> {
        let mcp = match &invocation.action.request {
            ToolRequest::Mcp(request) => request,
            ToolRequest::File(_)
            | ToolRequest::Git(_)
            | ToolRequest::Shell(_)
            | ToolRequest::Network(_) => {
                return Err(capability_rejection(
                    CapabilityRejectionCode::RequestFamilyMismatch,
                    "Worker Capability Adapter accepts MCP requests only",
                ));
            }
        };
        let actual_id = canonical_mcp_capability_id(&mcp.server, &mcp.tool)
            .map_err(|_| CapabilityAdapterError::InvalidMcpTarget)?;
        if actual_id != invocation.capability_id {
            return Err(capability_rejection(
                CapabilityRejectionCode::CapabilityTargetMismatch,
                "claimed capability does not match the actual MCP target",
            ));
        }
        let descriptor = self
            .catalog
            .capabilities
            .iter()
            .find(|capability| capability.id == actual_id)
            .ok_or_else(|| {
                capability_rejection(
                    CapabilityRejectionCode::UnknownCapability,
                    "MCP target is absent from current Worker discovery",
                )
            })?;
        if descriptor.version != invocation.capability_version {
            return Err(capability_rejection(
                CapabilityRejectionCode::CapabilityVersionMismatch,
                "claimed capability version differs from current Worker discovery",
            ));
        }
        if descriptor.health == CapabilityHealth::Unavailable {
            return Err(capability_rejection(
                CapabilityRejectionCode::CapabilityUnavailable,
                "capability health is unavailable",
            ));
        }
        if descriptor.origin == CapabilityOrigin::Unmanaged
            && self.unmanaged_policy == UnmanagedCapabilityPolicy::Deny
        {
            return Err(capability_rejection(
                CapabilityRejectionCode::UnmanagedCapabilityDenied,
                "current policy rejects unmanaged capabilities",
            ));
        }

        validate_grant(&self.catalog, &self.grants, descriptor, invocation.action)?;

        let mut warnings = Vec::with_capacity(2);
        if descriptor.origin == CapabilityOrigin::Unmanaged {
            warnings.push(CapabilityWarning::UnmanagedCapability);
        }
        if descriptor.health == CapabilityHealth::Degraded {
            warnings.push(CapabilityWarning::DegradedCapability);
        }
        let executed = self
            .gateway
            .execute(
                now,
                invocation.action,
                invocation.enforcement_receipt,
                verifier,
                receipt_store,
            )
            .map_err(|error| CapabilityAdapterError::Gateway(Box::new(error)))?;
        Ok(CapabilityInvocation { warnings, executed })
    }
}

fn validate_grant<RecorderError, ExecutorError>(
    catalog: &WorkerCapabilityCatalog,
    grants: &[CapabilityGrant],
    descriptor: &CapabilityDescriptor,
    action: &WorkerActionRequest,
) -> Result<(), CapabilityAdapterError<RecorderError, ExecutorError>> {
    let capability_grants: Vec<&CapabilityGrant> = grants
        .iter()
        .filter(|grant| grant.capability_id == descriptor.id)
        .collect();
    if capability_grants.is_empty() {
        return Err(capability_rejection(
            CapabilityRejectionCode::CapabilityNotAuthorized,
            "capability has no explicit authorization",
        ));
    }
    let session_grants: Vec<&CapabilityGrant> = capability_grants
        .into_iter()
        .filter(|grant| grant.worker_session_id == action.authority.worker_session_id)
        .collect();
    if session_grants.is_empty() {
        return Err(capability_rejection(
            CapabilityRejectionCode::StaleWorkerSession,
            "capability authorization belongs to another WorkerSession",
        ));
    }
    let envelope_grants: Vec<&CapabilityGrant> = session_grants
        .into_iter()
        .filter(|grant| grant.envelope == action.authority.envelope)
        .collect();
    if envelope_grants.is_empty() {
        return Err(capability_rejection(
            CapabilityRejectionCode::StaleExecutionEnvelope,
            "capability authorization belongs to another Execution Envelope",
        ));
    }
    if !envelope_grants.iter().any(|grant| {
        grant.catalog_digest == catalog.catalog_digest
            && grant.capability_version == descriptor.version
    }) {
        return Err(capability_rejection(
            CapabilityRejectionCode::StaleCapabilityCatalog,
            "capability authorization belongs to a replaced discovery snapshot",
        ));
    }
    Ok(())
}

fn validate_version(version: &str) -> Result<(), CapabilityCatalogError> {
    if version.is_empty()
        || version.len() > 128
        || !version
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b'+'))
    {
        return Err(catalog_error(
            CapabilityCatalogErrorCode::InvalidCapabilityVersion,
            "capability version must be a portable non-secret identifier",
        ));
    }
    Ok(())
}

fn is_sha256_digest(value: &str) -> bool {
    value.len() == 71
        && value.starts_with("sha256:")
        && value[7..]
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

fn catalog_error(code: CapabilityCatalogErrorCode, reason: &str) -> CapabilityCatalogError {
    CapabilityCatalogError {
        code,
        reason: reason.to_owned(),
    }
}

fn capability_rejection<RecorderError, ExecutorError>(
    code: CapabilityRejectionCode,
    reason: &str,
) -> CapabilityAdapterError<RecorderError, ExecutorError> {
    CapabilityAdapterError::Rejected {
        code,
        reason: reason.to_owned(),
    }
}
