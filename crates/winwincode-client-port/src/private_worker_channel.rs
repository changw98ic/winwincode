// SPDX-License-Identifier: Apache-2.0

use serde::{Deserialize, Serialize};
use winwincode_execution_port::agent_config::{AgentFusionSettings, AgentJevContextSettings};

use crate::worker_relay::{WorkerRelayRequest, WorkerRelayResponse};

pub const MAX_PRIVATE_LINE_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_PRIVATE_PENDING: usize = 64;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProviderExecutionProfile {
    pub provider: Option<String>,
    pub model: Option<String>,
    pub reasoning: Option<String>,
    pub fusion: Option<AgentFusionSettings>,
    pub jev_context: Option<AgentJevContextSettings>,
    pub jev_judge: Option<String>,
    #[serde(default)]
    pub observer_mode: winwincode_execution_port::runtime_trace_outbox::ObserverMode,
    #[serde(default)]
    pub observer_provider: Option<String>,
    #[serde(default)]
    pub observer_model: Option<String>,
    #[serde(default)]
    pub observer_capability: Option<String>,
    #[serde(default)]
    pub observer_route: Option<String>,
    #[serde(default)]
    pub sealed_benchmark_tools: bool,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PrivateWorkerRequest {
    pub request_id: u64,
    pub request: PrivateWorkerRequestBody,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase", deny_unknown_fields)]
pub enum PrivateWorkerRequestBody {
    Relay {
        request: WorkerRelayRequest,
    },
    Provider {
        operation: ProviderOperation,
        body: String,
    },
    RefreshExtensions {
        body: String,
    },
}

#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ProviderOperation {
    ModelParameters,
    JevParameters,
    ResolveConnection,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PrivateWorkerResponse {
    pub request_id: u64,
    pub response: PrivateWorkerResponseBody,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase", deny_unknown_fields)]
pub enum PrivateWorkerResponseBody {
    Relay {
        response: WorkerRelayResponse,
    },
    Local {
        payload: SensitivePayload,
        error: Option<String>,
    },
}

#[derive(Serialize, Deserialize)]
#[serde(transparent)]
pub struct SensitivePayload(pub Vec<u8>);

impl Drop for SensitivePayload {
    fn drop(&mut self) {
        self.0.fill(0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_provider_request_is_not_a_server_relay_request() {
        let request = PrivateWorkerRequest {
            request_id: 7,
            request: PrivateWorkerRequestBody::Provider {
                operation: ProviderOperation::ResolveConnection,
                body: "{}".into(),
            },
        };
        let encoded = serde_json::to_vec(&request).unwrap();
        assert!(serde_json::from_slice::<WorkerRelayRequest>(&encoded).is_err());
        assert!(
            serde_json::from_slice::<crate::worker_relay::DeviceWorkerRelayRequest>(&encoded)
                .is_err()
        );
        assert_eq!(
            serde_json::from_slice::<PrivateWorkerRequest>(&encoded)
                .unwrap()
                .request_id,
            7
        );
    }
}
