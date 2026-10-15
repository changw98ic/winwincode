// SPDX-License-Identifier: Apache-2.0

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum WorkerRelayRequest {
    Exchange { body: String },
    Accounting { body: Option<String>, offset: u64 },
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkerRelayResponse {
    pub status: u16,
    pub body: String,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceWorkerRelayRequest {
    pub client_node_id: String,
    pub worker_id: String,
    pub worker_instance_id: String,
    pub request: WorkerRelayRequest,
}
