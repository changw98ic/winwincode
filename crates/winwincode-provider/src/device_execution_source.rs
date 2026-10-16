// SPDX-License-Identifier: Apache-2.0

#[cfg(test)]
#[path = "device_connection_snapshot_tests.rs"]
mod tests;

use std::{collections::BTreeMap, fmt, path::PathBuf};

use serde::{Deserialize, Serialize};
use winwincode_api::generated::{
    DeviceProviderConfig, DeviceProviderProtocol, DeviceResponsesStructuredOutput,
};
use winwincode_domain::{ModelExchangeId, RequestId, SessionIdentity, WorkerSessionId};
use winwincode_execution_port::generated::{ExecutionLeaseStamp, ModelOpenMessage};

use crate::{DeviceProviderError, DeviceProviderStore, OpenJevRemoteSettings, ResolvedSecret};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProviderExecutionContext {
    pub lease: ExecutionLeaseStamp,
    pub worker_session_id: WorkerSessionId,
    pub session_identity: SessionIdentity,
    pub model_exchange_id: ModelExchangeId,
    pub request_id: RequestId,
}

impl From<&ModelOpenMessage> for ProviderExecutionContext {
    fn from(open: &ModelOpenMessage) -> Self {
        Self {
            lease: open.lease.clone(),
            worker_session_id: open.worker_session_id.clone(),
            session_identity: open.session_identity.clone(),
            model_exchange_id: open.model_exchange_id.clone(),
            request_id: open.request_id.clone(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ProviderExecutionPurpose {
    Model,
    Observer,
    Fusion { member_id: String },
    JevContext { operation_id: String },
    JevJudge { operation_id: String },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProviderExecutionRequest {
    pub context: Option<ProviderExecutionContext>,
    pub provider_id: String,
    pub model_id: Option<String>,
    pub purpose: ProviderExecutionPurpose,
    pub physical_attempt: Option<i64>,
    pub adapter_request_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProviderModelParameters {
    pub provider_id: String,
    pub endpoint: String,
    pub protocol: DeviceProviderProtocol,
    pub responses_structured_output: Option<DeviceResponsesStructuredOutput>,
}

impl ProviderModelParameters {
    pub(crate) fn adapter_config(&self, model: &str) -> DeviceProviderConfig {
        DeviceProviderConfig {
            display_name: self.provider_id.clone(),
            enabled: true,
            endpoint: self.endpoint.clone(),
            model_ids: vec![model.to_owned()],
            protocol: self.protocol.clone(),
            provider_id: self.provider_id.clone(),
            responses_structured_output: self.responses_structured_output.clone(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProviderJevParameters {
    pub provider_id: String,
    pub endpoint: String,
    pub model_id: Option<String>,
    pub timeout_ms: u64,
    pub retries: u8,
    pub max_batch_size: Option<usize>,
    pub devices: Option<Vec<String>>,
    pub dtypes: Option<Vec<String>>,
}

impl ProviderJevParameters {
    pub fn settings(&self) -> OpenJevRemoteSettings {
        OpenJevRemoteSettings {
            provider_id: self.provider_id.clone(),
            endpoint: self.endpoint.clone(),
            api_key: None,
            model_id: self.model_id.clone(),
            timeout_ms: self.timeout_ms,
            retries: self.retries,
            max_batch_size: self.max_batch_size,
            devices: self.devices.clone(),
            dtypes: self.dtypes.clone(),
        }
    }
}

impl From<&OpenJevRemoteSettings> for ProviderJevParameters {
    fn from(settings: &OpenJevRemoteSettings) -> Self {
        Self {
            provider_id: settings.provider_id.clone(),
            endpoint: settings.endpoint.clone(),
            model_id: settings.model_id.clone(),
            timeout_ms: settings.timeout_ms,
            retries: settings.retries,
            max_batch_size: settings.max_batch_size,
            devices: settings.devices.clone(),
            dtypes: settings.dtypes.clone(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "parameters",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum ProviderConnectionParameters {
    Model(ProviderModelParameters),
    Jev(ProviderJevParameters),
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProviderConnectionMaterial {
    pub parameters: ProviderConnectionParameters,
    pub secret: Option<Vec<u8>>,
    pub custom_headers: BTreeMap<String, String>,
    pub codex_account: Option<String>,
}

impl fmt::Debug for ProviderConnectionMaterial {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ProviderConnectionMaterial([REDACTED])")
    }
}

impl Drop for ProviderConnectionMaterial {
    fn drop(&mut self) {
        if let Some(secret) = &mut self.secret {
            secret.fill(0);
        }
        for value in self.custom_headers.values_mut() {
            clear_string(value);
        }
        if let Some(account) = &mut self.codex_account {
            clear_string(account);
        }
    }
}

fn clear_string(value: &mut String) {
    let mut bytes = std::mem::take(value).into_bytes();
    bytes.fill(0);
}

impl ProviderConnectionMaterial {
    pub(crate) fn take_secret(&mut self) -> Result<Option<ResolvedSecret>, DeviceProviderError> {
        self.secret
            .take()
            .map(|bytes| {
                ResolvedSecret::from_bytes(bytes).map_err(|_| DeviceProviderError::StateInvalid)
            })
            .transpose()
    }
}

pub trait ProviderExecutionSource: fmt::Debug + Send + Sync {
    fn model_parameters(
        &self,
        request: &ProviderExecutionRequest,
    ) -> Result<ProviderModelParameters, DeviceProviderError>;

    fn jev_parameters(
        &self,
        request: &ProviderExecutionRequest,
    ) -> Result<ProviderJevParameters, DeviceProviderError>;

    fn resolve_connection(
        &self,
        request: &ProviderExecutionRequest,
        can_start: &dyn Fn() -> bool,
    ) -> Result<ProviderConnectionMaterial, DeviceProviderError>;
}

#[derive(Clone)]
pub struct LocalDeviceProviderSource {
    directory: PathBuf,
}

impl fmt::Debug for LocalDeviceProviderSource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("LocalDeviceProviderSource([REDACTED])")
    }
}

impl LocalDeviceProviderSource {
    pub fn new(directory: PathBuf) -> Self {
        Self { directory }
    }
}

impl ProviderExecutionSource for LocalDeviceProviderSource {
    fn model_parameters(
        &self,
        request: &ProviderExecutionRequest,
    ) -> Result<ProviderModelParameters, DeviceProviderError> {
        let store = DeviceProviderStore::open(&self.directory)?;
        let saved: String = store.connection.query_row(
            "SELECT config FROM providers WHERE provider_id=?1",
            [&request.provider_id],
            |row| row.get(0),
        )?;
        let config: DeviceProviderConfig = serde_json::from_str(&saved)?;
        let model = request
            .model_id
            .as_deref()
            .ok_or(DeviceProviderError::StateInvalid)?;
        if !crate::device_store::valid_device_provider_config(&config)
            || !config.enabled
            || config.provider_id != request.provider_id
            || !config.model_ids.iter().any(|selected| selected == model)
        {
            return Err(DeviceProviderError::StateInvalid);
        }
        Ok(ProviderModelParameters {
            provider_id: config.provider_id,
            endpoint: config.endpoint,
            protocol: config.protocol,
            responses_structured_output: config.responses_structured_output,
        })
    }

    fn jev_parameters(
        &self,
        request: &ProviderExecutionRequest,
    ) -> Result<ProviderJevParameters, DeviceProviderError> {
        let store = DeviceProviderStore::open(&self.directory)?;
        let mut settings = store.resolve_jev_settings(&request.provider_id)?;
        let parameters = ProviderJevParameters::from(&settings);
        if let Some(mut key) = settings.api_key.take() {
            clear_string(&mut key);
        }
        Ok(parameters)
    }

    fn resolve_connection(
        &self,
        request: &ProviderExecutionRequest,
        can_start: &dyn Fn() -> bool,
    ) -> Result<ProviderConnectionMaterial, DeviceProviderError> {
        if !can_start() {
            return Err(DeviceProviderError::AuthorityEnded);
        }
        let store = DeviceProviderStore::open(&self.directory)?;
        let material = store.execution_connection(request)?;
        if !can_start() {
            return Err(DeviceProviderError::AuthorityEnded);
        }
        Ok(material)
    }
}

impl DeviceProviderStore {
    fn execution_connection(
        &self,
        request: &ProviderExecutionRequest,
    ) -> Result<ProviderConnectionMaterial, DeviceProviderError> {
        let material = match &request.purpose {
            ProviderExecutionPurpose::Model
            | ProviderExecutionPurpose::Observer
            | ProviderExecutionPurpose::Fusion { .. } => {
                let model = request
                    .model_id
                    .as_deref()
                    .ok_or(DeviceProviderError::StateInvalid)?;
                let (config, secret, codex_account) =
                    self.resolve_connection(&request.provider_id)?;
                if !config.enabled || !config.model_ids.iter().any(|selected| selected == model) {
                    return Err(DeviceProviderError::StateInvalid);
                }
                let transaction = self.connection.unchecked_transaction()?;
                let (saved_config, saved_secret): (String, Vec<u8>) = transaction.query_row(
                    "SELECT config,secret FROM providers WHERE provider_id=?1",
                    [&request.provider_id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )?;
                let saved_secret = ResolvedSecret::from_bytes(saved_secret)
                    .map_err(|_| DeviceProviderError::StateInvalid)?;
                let latest: DeviceProviderConfig = serde_json::from_str(&saved_config)?;
                let headers = self.custom_headers(&request.provider_id)?;
                transaction.commit()?;
                let same_authorization = match &config.protocol {
                    DeviceProviderProtocol::CodexChatgpt => {
                        serde_json::from_slice::<crate::codex_login::CodexLoginBinding>(
                            saved_secret.expose(),
                        )?
                        .resolve()
                        .is_ok_and(|(latest, account)| {
                            latest.expose() == secret.expose()
                                && codex_account.as_deref() == Some(account.as_str())
                        })
                    }
                    DeviceProviderProtocol::ChatgptPlan => {
                        serde_json::from_slice::<crate::chatgpt_oauth::Credentials>(
                            saved_secret.expose(),
                        )?
                        .secret()
                        .is_ok_and(|latest| latest.expose() == secret.expose())
                    }
                    _ => saved_secret.expose() == secret.expose(),
                };
                if !latest.enabled
                    || latest.provider_id != config.provider_id
                    || latest.endpoint != config.endpoint
                    || latest.protocol != config.protocol
                    || latest.responses_structured_output != config.responses_structured_output
                    || !latest.model_ids.iter().any(|selected| selected == model)
                    || !same_authorization
                {
                    return Err(DeviceProviderError::StateInvalid);
                }
                ProviderConnectionMaterial {
                    parameters: ProviderConnectionParameters::Model(ProviderModelParameters {
                        provider_id: config.provider_id,
                        endpoint: config.endpoint,
                        protocol: config.protocol,
                        responses_structured_output: config.responses_structured_output,
                    }),
                    secret: Some(secret.expose().to_vec()),
                    custom_headers: headers,
                    codex_account,
                }
            }
            ProviderExecutionPurpose::JevContext { .. }
            | ProviderExecutionPurpose::JevJudge { .. } => {
                let mut settings = self.resolve_jev_settings(&request.provider_id)?;
                let parameters = ProviderJevParameters::from(&settings);
                let secret = settings.api_key.take().map(String::into_bytes);
                ProviderConnectionMaterial {
                    parameters: ProviderConnectionParameters::Jev(parameters),
                    secret,
                    custom_headers: BTreeMap::new(),
                    codex_account: None,
                }
            }
        };
        Ok(material)
    }
}
