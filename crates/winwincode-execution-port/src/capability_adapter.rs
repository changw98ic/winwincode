// SPDX-License-Identifier: Apache-2.0

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

pub const CAPABILITY_ADAPTER_VERSION: &str = "winwincode-worker-capability/v1";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityOrigin {
    CodexCoreMcp,
    MappedPluginManifest,
    Unmanaged,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityHealth {
    Healthy,
    Degraded,
    Unavailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnmanagedCapabilityPolicy {
    Warn,
    Deny,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapabilityCatalogErrorCode {
    InvalidRegistryCapabilityDigest,
    McpFeatureNotRegistered,
    InvalidCapabilityVersion,
    DuplicateCapability,
    CatalogDigestFailure,
    UnknownCapability,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapabilityCatalogError {
    pub code: CapabilityCatalogErrorCode,
    pub reason: String,
}

impl fmt::Display for CapabilityCatalogError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.reason)
    }
}

impl std::error::Error for CapabilityCatalogError {}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub struct CapabilityDescriptor {
    id: String,
    version: String,
    health: CapabilityHealth,
    origin: CapabilityOrigin,
}

impl CapabilityDescriptor {
    pub fn mcp_resource(
        server: &str,
        operation: crate::mcp_resource::McpResourceOperation,
        version: &str,
        health: CapabilityHealth,
        origin: CapabilityOrigin,
    ) -> Result<Self, CapabilityDescriptorError> {
        validate_version(version).map_err(CapabilityDescriptorError::Catalog)?;
        let id = crate::mcp_resource::canonical_resource_capability_id(server, operation)
            .map_err(|_| CapabilityDescriptorError::InvalidMcpTarget)?;
        Ok(Self {
            id,
            version: version.to_owned(),
            health,
            origin,
        })
    }

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

    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    #[must_use]
    pub fn version(&self) -> &str {
        &self.version
    }

    #[must_use]
    pub const fn health(&self) -> CapabilityHealth {
        self.health
    }

    #[must_use]
    pub const fn origin(&self) -> CapabilityOrigin {
        self.origin
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CapabilityDescriptorError {
    InvalidMcpTarget,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerCapabilityCatalog {
    registry_capability_digest: Sha256Digest,
    catalog_digest: Sha256Digest,
    capabilities: Vec<CapabilityDescriptor>,
}

impl WorkerCapabilityCatalog {
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

    #[must_use]
    pub const fn adapter_version(&self) -> &'static str {
        CAPABILITY_ADAPTER_VERSION
    }

    #[must_use]
    pub const fn registry_capability_digest(&self) -> &Sha256Digest {
        &self.registry_capability_digest
    }

    #[must_use]
    pub const fn catalog_digest(&self) -> &Sha256Digest {
        &self.catalog_digest
    }

    #[must_use]
    pub fn capabilities(&self) -> &[CapabilityDescriptor] {
        &self.capabilities
    }

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapabilityGrant {
    capability_id: String,
    capability_version: String,
    catalog_digest: Sha256Digest,
    worker_session_id: WorkerSessionId,
    envelope: ExecutionEnvelopeToken,
}

impl CapabilityGrant {
    #[must_use]
    pub fn capability_id(&self) -> &str {
        &self.capability_id
    }

    #[must_use]
    pub fn capability_version(&self) -> &str {
        &self.capability_version
    }

    #[must_use]
    pub const fn worker_session_id(&self) -> &WorkerSessionId {
        &self.worker_session_id
    }

    #[must_use]
    pub const fn envelope(&self) -> &ExecutionEnvelopeToken {
        &self.envelope
    }
}

#[derive(Debug, Clone, Copy)]
pub struct CapabilityInvocationRequest<'action> {
    pub capability_id: &'action str,
    pub capability_version: &'action str,
    pub action: &'action WorkerActionRequest,
    pub enforcement_receipt: &'action ActionEnforcementReceiptMessage,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapabilityWarning {
    UnmanagedCapability,
    DegradedCapability,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CapabilityInvocation<Output> {
    pub warnings: Vec<CapabilityWarning>,
    pub executed: ExecutedAction<Output>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapabilityRejectionCode {
    RequestFamilyMismatch,
    CapabilityTargetMismatch,
    UnknownCapability,
    CapabilityVersionMismatch,
    CapabilityUnavailable,
    UnmanagedCapabilityDenied,
    CapabilityNotAuthorized,
    StaleWorkerSession,
    StaleExecutionEnvelope,
    StaleCapabilityCatalog,
}

#[derive(Debug, Clone, PartialEq)]
pub enum CapabilityAdapterError<RecorderError, ExecutorError> {
    Rejected {
        code: CapabilityRejectionCode,
        reason: String,
    },
    InvalidMcpTarget,
    Gateway(Box<ActionGatewayError<RecorderError, ExecutorError>>),
}

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

    pub fn replace_catalog(&mut self, catalog: WorkerCapabilityCatalog) {
        self.catalog = catalog;
    }

    pub fn replace_grants(&mut self, grants: Vec<CapabilityGrant>) {
        self.grants = grants;
    }

    #[must_use]
    pub const fn catalog(&self) -> &WorkerCapabilityCatalog {
        &self.catalog
    }

    pub fn invoke(
        &mut self,
        now: &Instant,
        invocation: CapabilityInvocationRequest<'_>,
        verifier: &ActionEnforcementVerifier,
        receipt_store: &mut dyn ActionReceiptUseStore,
    ) -> CapabilityAdapterResult<Executor::Output, Recorder::Error, Executor::Error> {
        let actual_id = match &invocation.action.request {
            ToolRequest::Mcp(request) => {
                canonical_mcp_capability_id(&request.server, &request.tool)
            }
            ToolRequest::McpResource(request) => {
                crate::mcp_resource::canonical_resource_capability_id(
                    &request.server,
                    request.operation,
                )
            }
            ToolRequest::File(_)
            | ToolRequest::Git(_)
            | ToolRequest::Shell(_)
            | ToolRequest::Network(_)
            | ToolRequest::ProcessInput(_) => {
                return Err(capability_rejection(
                    CapabilityRejectionCode::RequestFamilyMismatch,
                    "Worker Capability Adapter accepts MCP requests only",
                ));
            }
        }
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
