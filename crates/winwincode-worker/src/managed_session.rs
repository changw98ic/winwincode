// SPDX-License-Identifier: Apache-2.0

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use winwincode_client_port::private_worker_channel::ProviderExecutionProfile;
use winwincode_domain::{
    ClientInstanceId, ClientNodeId, ClientOccupancyLeaseId, ProductSessionId, RepositoryBindingId,
    WorkRunId, WorkerId, WorkerInstanceId, WorkerSessionId,
};
use winwincode_execution_port::generated::ModelGatewayRoute;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedSessionConfigError {
    pub reason: String,
}

impl fmt::Display for ManagedSessionConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "managed session config: {}", self.reason)
    }
}

impl std::error::Error for ManagedSessionConfigError {}

#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ManagedSessionConfig {
    pub client_node_id: ClientNodeId,
    pub client_instance_id: ClientInstanceId,
    pub occupancy_lease_id: ClientOccupancyLeaseId,
    #[serde(deserialize_with = "deserialize_fencing_token")]
    pub occupancy_fencing_token: u64,
    pub repository_binding_id: RepositoryBindingId,
    pub product_session_id: Option<ProductSessionId>,
    pub work_run_id: Option<WorkRunId>,
    pub worker_session_id: WorkerSessionId,
    pub worker_id: WorkerId,
    pub worker_instance_id: WorkerInstanceId,
    pub source_directory: PathBuf,
    pub data_directory: PathBuf,
    pub execution_workspace_directory: Option<PathBuf>,
    pub provider_runtime_directory: PathBuf,
    pub provider_execution_profile: ProviderExecutionProfile,
    pub helper_executable: PathBuf,
    pub model_route: Option<ModelGatewayRoute>,
}

fn deserialize_fencing_token<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<u64, D::Error> {
    String::deserialize(deserializer)?
        .parse()
        .map_err(serde::de::Error::custom)
}

impl ManagedSessionConfig {
    pub fn read(path: &Path) -> Result<Self, ManagedSessionConfigError> {
        let bytes = fs::read(path).map_err(|error| ManagedSessionConfigError {
            reason: format!("cannot read {}: {error}", path.display()),
        })?;
        serde_json::from_slice(&bytes).map_err(|error| ManagedSessionConfigError {
            reason: error.to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> serde_json::Value {
        serde_json::json!({
            "clientNodeId": "device",
            "clientInstanceId": "instance",
            "occupancyLeaseId": "lease",
            "occupancyFencingToken": "7",
            "repositoryBindingId": "repository",
            "workerSessionId": "session",
            "workerId": "worker",
            "workerInstanceId": "worker-instance",
            "sourceDirectory": "/repo",
            "dataDirectory": "/data/worker",
            "providerRuntimeDirectory": "/data/providers/runtime",
            "providerExecutionProfile": {},
            "helperExecutable": "/bin/winwincode-kernel-helper"
        })
    }

    #[test]
    fn reads_the_device_configuration_without_a_provider_or_credential_preflight() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("session.json");
        fs::write(&path, config().to_string()).unwrap();
        let config = ManagedSessionConfig::read(&path).unwrap();
        assert_eq!(config.worker_id.0, "worker");
        assert_eq!(config.occupancy_fencing_token, 7);
        assert_eq!(
            config.helper_executable,
            PathBuf::from("/bin/winwincode-kernel-helper")
        );
        assert!(!config.provider_runtime_directory.exists());
    }

    #[test]
    fn rejects_a_secret_store_path_in_the_worker_configuration() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("session.json");
        let mut config = config();
        config["providerDirectory"] = "/device/provider-secrets".into();
        fs::write(&path, config.to_string()).unwrap();
        let error = ManagedSessionConfig::read(&path).unwrap_err().to_string();
        assert!(error.contains("providerDirectory"));
        assert!(!error.contains("/device/provider-secrets"));
    }

    #[test]
    fn reports_configuration_parse_failures() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("session.json");
        let mut config = config();
        config.as_object_mut().unwrap().remove("helperExecutable");
        fs::write(&path, config.to_string()).unwrap();
        assert!(
            ManagedSessionConfig::read(&path)
                .unwrap_err()
                .to_string()
                .contains("helperExecutable")
        );
        fs::write(&path, "{").unwrap();
        assert!(ManagedSessionConfig::read(&path).is_err());
    }
}
