// SPDX-License-Identifier: Apache-2.0
use super::{
    ProductionCodexConfig, ProductionCodexError, ProductionCodexErrorKind, invalid_configuration,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use winwincode_domain::Sha256Digest;
use winwincode_execution_port::agent_config::AgentSessionConfigSnapshot;

/// Immutable product composition retained beside the original Core rollout.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct ToolRuntimeContract {
    schema_version: u32,
    kernel_interface_version: u32,
    runtime_digest: Sha256Digest,
    host_binary_digest: Sha256Digest,
    catalog_digest: Sha256Digest,
    policy_digest: Sha256Digest,
}
impl ToolRuntimeContract {
    pub(super) fn capture(
        config: &ProductionCodexConfig,
        profile: &AgentSessionConfigSnapshot,
    ) -> Result<Self, ProductionCodexError> {
        let mut discovered = config.discovered_capabilities.clone();
        discovered.sort_unstable();
        let catalog = serde_json::to_vec(&(&config.registered_capabilities, discovered))
            .map_err(|_| invalid_configuration())?;
        Ok(Self {
            schema_version: 1,
            kernel_interface_version: winwincode_kernel::INTERFACE_VERSION,
            runtime_digest: Sha256Digest(format!(
                "sha256:{:x}",
                Sha256::digest(include_bytes!("../../../upstream/sources.lock.json"))
            )),
            host_binary_digest: config.helper_release_manifest.binary_digest().clone(),
            catalog_digest: Sha256Digest(format!("sha256:{:x}", Sha256::digest(catalog))),
            policy_digest: profile.snapshot_digest.clone(),
        })
    }
    pub(super) fn validate_resume(&self, saved: Option<&Self>) -> Result<(), ProductionCodexError> {
        if saved == Some(self) {
            return Ok(());
        }
        Err(ProductionCodexError::new(
            ProductionCodexErrorKind::Restart,
            "task runtime, tool catalog or policy differs from its original snapshot",
        ))
    }
}
#[cfg(test)]
#[path = "tool_runtime_contract_tests.rs"]
mod tests;
