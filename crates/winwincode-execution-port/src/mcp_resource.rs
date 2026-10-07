// SPDX-License-Identifier: Apache-2.0

//! Resource methods retain their protocol identity independently of MCP tools.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::action_normalizer::{
    ActionNormalizationError, ActionObject, ActionOperation, ActionRisk, ActionScope, ActionSource,
    ObservedAction, ObservedFact, canonical_mcp_capability_id,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum McpResourceOperation {
    List,
    ListTemplates,
    Read,
}

impl McpResourceOperation {
    pub const ALL: [Self; 3] = [Self::List, Self::ListTemplates, Self::Read];

    #[must_use]
    pub const fn method(self) -> &'static str {
        match self {
            Self::List => "resources/list",
            Self::ListTemplates => "resources/templates/list",
            Self::Read => "resources/read",
        }
    }

    #[must_use]
    pub fn from_method(method: &str) -> Option<Self> {
        match method {
            "resources/list" => Some(Self::List),
            "resources/templates/list" => Some(Self::ListTemplates),
            "resources/read" => Some(Self::Read),
            _ => None,
        }
    }
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct McpResourceRequest {
    pub server: String,
    pub operation: McpResourceOperation,
    pub arguments: Value,
}

impl std::fmt::Debug for McpResourceRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpResourceRequest")
            .field("server", &self.server)
            .field("operation", &self.operation)
            .field("arguments", &"<private>")
            .finish()
    }
}

/// Resource capabilities use a distinct scheme so an identically named tool
/// cannot acquire or revoke a resource-method grant.
///
/// # Errors
/// Returns a validation error if the configured server name is invalid.
pub fn canonical_resource_capability_id(
    server: &str,
    operation: McpResourceOperation,
) -> Result<String, ActionNormalizationError> {
    let target = canonical_mcp_capability_id(server, operation.method())?;
    Ok(target.replacen("mcp://", "mcp-resource://", 1))
}

pub(crate) fn observe(
    request: &McpResourceRequest,
) -> Result<ObservedAction, ActionNormalizationError> {
    Ok(ObservedAction {
        source: ActionSource::Mcp,
        objects: vec![ActionObject::ExternalResource],
        operation: ActionOperation::Execute,
        scope: ActionScope::External,
        targets: vec![canonical_resource_capability_id(
            &request.server,
            request.operation,
        )?],
        minimum_risk: ActionRisk::Low,
        facts: vec![ObservedFact::McpCapability],
    })
}
