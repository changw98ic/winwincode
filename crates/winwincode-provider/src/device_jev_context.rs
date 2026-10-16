// SPDX-License-Identifier: Apache-2.0

use rusqlite::{OptionalExtension, params};
use sha2::{Digest, Sha256};
use winwincode_execution_port::jev_decision::{JevPolicy, validate_policy};

use crate::{
    DeviceProviderError, DeviceProviderStore, JevContextEvaluation, JevContextRequest,
    JevExecutionOptions, JevRun, JevRuntime, ProviderRuntimeStore,
};

#[cfg(test)]
#[path = "device_jev_chat_scope_tests.rs"]
mod chat_scope_tests;

pub(crate) const MAX_JEV_REMOTE_REQUEST_BYTES: usize = 90_000;

#[derive(Debug, Clone, PartialEq)]
pub enum StoredJevContext {
    Completed {
        run: Box<JevRun<JevContextEvaluation>>,
        replayed: bool,
    },
    Incomplete,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DeviceJevReceipt<T = JevContextEvaluation> {
    pub operation_id: String,
    pub input_digest: String,
    pub run: Option<JevRun<T>>,
}

impl ProviderRuntimeStore {
    pub fn model_jev_source_item(
        &self,
        open: &winwincode_execution_port::generated::ModelOpenMessage,
        operation_id: &str,
    ) -> Result<serde_json::Value, DeviceProviderError> {
        if !self
            .model_jev_receipts(open)?
            .iter()
            .any(|receipt| receipt.operation_id == operation_id && receipt.run.is_some())
        {
            return Err(DeviceProviderError::StateInvalid);
        }
        let index: usize = operation_id
            .strip_prefix(&format!("jev:{}:", open.model_exchange_id.0))
            .ok_or(DeviceProviderError::StateInvalid)?
            .parse()
            .map_err(|_| DeviceProviderError::StateInvalid)?;
        let request: serde_json::Value =
            serde_json::from_slice(&crate::device_model::validated_model_payload(open)?)?;
        let source = request
            .pointer("/request/input")
            .and_then(serde_json::Value::as_array)
            .and_then(|items| items.get(index))
            .filter(|item| is_assistant_commentary(item))
            .cloned()
            .ok_or(DeviceProviderError::StateInvalid)?;
        let scored_request: Option<String> = self.connection.query_row(
            "SELECT request_json FROM jev_context_exchanges WHERE operation_id=?1",
            [operation_id],
            |row| row.get(0),
        )?;
        let scored: serde_json::Value =
            serde_json::from_str(&scored_request.ok_or(DeviceProviderError::StateInvalid)?)?;
        let candidate: serde_json::Value = serde_json::from_str(
            scored
                .pointer("/2/candidate")
                .and_then(serde_json::Value::as_str)
                .ok_or(DeviceProviderError::StateInvalid)?,
        )?;
        if candidate != source {
            return Err(DeviceProviderError::StateInvalid);
        }
        Ok(source)
    }

    pub fn model_jev_receipts(
        &self,
        open: &winwincode_execution_port::generated::ModelOpenMessage,
    ) -> Result<Vec<DeviceJevReceipt>, DeviceProviderError> {
        let request = serde_json::to_string(open)?;
        let (digest, saved): (String, Option<String>) = self.connection.query_row(
            "SELECT digest, request_open FROM exchanges WHERE exchange_id=?1",
            [&open.model_exchange_id.0],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        if saved.as_deref() != Some(request.as_str())
            || digest != format!("{:x}", Sha256::digest(request.as_bytes()))
        {
            return Err(DeviceProviderError::StateInvalid);
        }
        let prefix = format!("jev:{}:", open.model_exchange_id.0);
        let mut statement = self.connection.prepare(
            "SELECT operation_id,digest,result,request_json FROM jev_context_exchanges
             WHERE substr(operation_id,1,length(?1))=?1 ORDER BY operation_id",
        )?;
        let rows = statement.query_map([&prefix], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<String>>(3)?,
            ))
        })?;
        rows.map(|row| {
            let (operation_id, input_digest, result, request_json) = row?;
            if request_json.as_ref().is_some_and(|request| {
                format!("{:x}", Sha256::digest(request.as_bytes())) != input_digest
            }) {
                return Err(DeviceProviderError::StateInvalid);
            }
            let index = operation_id
                .strip_prefix(&prefix)
                .ok_or(DeviceProviderError::StateInvalid)?;
            if index.is_empty()
                || !index.bytes().all(|byte| byte.is_ascii_digit())
                || input_digest.len() != 64
                || !input_digest.bytes().all(|byte| byte.is_ascii_hexdigit())
            {
                return Err(DeviceProviderError::StateInvalid);
            }
            Ok(DeviceJevReceipt {
                operation_id,
                input_digest,
                run: result.as_deref().map(serde_json::from_str).transpose()?,
            })
        })
        .collect()
    }
}

impl DeviceProviderStore {
    pub fn save_jev_settings(
        &self,
        settings: &crate::OpenJevRemoteSettings,
    ) -> Result<(), DeviceProviderError> {
        let settings = validated_system_one_settings(settings.clone())?;
        self.connection.execute(
            "INSERT INTO jev_settings VALUES (?1, ?2) ON CONFLICT(provider_id) DO UPDATE SET settings=excluded.settings",
            params![settings.provider_id, serde_json::to_string(&settings)?],
        )?;
        Ok(())
    }

    pub fn import_jev_settings(
        &self,
        path: &std::path::Path,
        provider: &str,
    ) -> Result<(), DeviceProviderError> {
        use std::io::Read;
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let before = std::fs::symlink_metadata(path)?;
        if !before.is_file() || before.permissions().mode() & 0o077 != 0 {
            return Err(DeviceProviderError::StateInvalid);
        }
        let file = std::fs::File::open(path)?;
        let opened = file.metadata()?;
        if !opened.is_file()
            || opened.permissions().mode() & 0o077 != 0
            || (before.dev(), before.ino()) != (opened.dev(), opened.ino())
        {
            return Err(DeviceProviderError::StateInvalid);
        }
        let mut input = String::new();
        file.take(65_537).read_to_string(&mut input)?;
        if input.len() > 65_536 {
            return Err(DeviceProviderError::StateInvalid);
        }
        let settings = crate::OpenJevRemoteSettings::from_toml(&input)
            .map_err(|_| DeviceProviderError::StateInvalid)?;
        if settings.provider_id != provider {
            return Err(DeviceProviderError::StateInvalid);
        }
        self.save_jev_settings(&settings)
    }

    pub fn resolve_jev_settings(
        &self,
        provider: &str,
    ) -> Result<crate::OpenJevRemoteSettings, DeviceProviderError> {
        let saved: String = self.connection.query_row(
            "SELECT settings FROM jev_settings WHERE provider_id=?1",
            [provider],
            |row| row.get(0),
        )?;
        let settings: crate::OpenJevRemoteSettings = serde_json::from_str(&saved)?;
        if settings.provider_id != provider {
            return Err(DeviceProviderError::StateInvalid);
        }
        validated_system_one_settings(settings)
    }
}

impl ProviderRuntimeStore {
    pub(crate) fn jev_execution_request(
        &self,
        operation_id: &str,
        provider: &str,
        judge: bool,
    ) -> Result<crate::device_execution_source::ProviderExecutionRequest, DeviceProviderError> {
        let context = if let Some(operation) =
            operation_id.strip_prefix(if judge { "judge:" } else { "jev:" })
        {
            let (exchange, _) = operation
                .rsplit_once(':')
                .ok_or(DeviceProviderError::StateInvalid)?;
            let saved: Option<String> = self
                .connection
                .query_row(
                    "SELECT request_open FROM exchanges WHERE exchange_id=?1",
                    [exchange],
                    |row| row.get(0),
                )
                .optional()?
                .flatten();
            saved
                .map(|saved| {
                    serde_json::from_str::<winwincode_execution_port::generated::ModelOpenMessage>(
                        &saved,
                    )
                })
                .transpose()?
                .as_ref()
                .map(Into::into)
        } else {
            None
        };
        Ok(crate::device_execution_source::ProviderExecutionRequest {
            context,
            provider_id: provider.to_owned(),
            model_id: None,
            purpose: if judge {
                crate::device_execution_source::ProviderExecutionPurpose::JevJudge {
                    operation_id: operation_id.to_owned(),
                }
            } else {
                crate::device_execution_source::ProviderExecutionPurpose::JevContext {
                    operation_id: operation_id.to_owned(),
                }
            },
            physical_attempt: None,
            adapter_request_id: None,
        })
    }

    pub async fn evaluate_configured_context_once(
        &self,
        operation_id: &str,
        session: &winwincode_execution_port::agent_config::AgentSessionConfigSnapshot,
        input: JevContextRequest,
    ) -> Result<StoredJevContext, DeviceProviderError> {
        self.evaluate_configured_context_once_authorized(operation_id, session, input, &|| true)
            .await
    }

    async fn evaluate_configured_context_once_authorized(
        &self,
        operation_id: &str,
        session: &winwincode_execution_port::agent_config::AgentSessionConfigSnapshot,
        input: JevContextRequest,
        can_start: &(impl Fn() -> bool + ?Sized),
    ) -> Result<StoredJevContext, DeviceProviderError> {
        use std::sync::Arc;
        use winwincode_execution_port::agent_config::validate_agent_session_config;
        validate_agent_session_config(session).map_err(|_| DeviceProviderError::StateInvalid)?;
        let context = session
            .profile
            .source
            .settings
            .jev_context
            .as_ref()
            .ok_or(DeviceProviderError::StateInvalid)?;
        let execution_request =
            self.jev_execution_request(operation_id, &context.provider, false)?;
        let parameters = self
            .execution_source()?
            .jev_parameters(&execution_request)?;
        let settings = parameters.settings();
        let (config, retry) = settings
            .to_config_and_runtime()
            .map_err(|_| DeviceProviderError::StateInvalid)?;
        let cancellation = self.jev_cancellation(operation_id)?;
        let transport = crate::device_store::device_jev_transport(&config)?
            .with_cancellation(cancellation.clone());
        let configuration_digest = configured_context_digest(session, &settings)?;
        let runtime = JevRuntime::new(
            vec![Arc::new(crate::OpenJevRemoteProvider::new(
                config,
                Arc::new(transport),
            ))],
            retry,
        )
        .with_cancellation(cancellation)
        .with_execution_source(
            self.execution_source
                .clone()
                .ok_or(DeviceProviderError::StateInvalid)?,
            execution_request,
            parameters,
        );
        self.evaluate_context_once_authorized(
            operation_id,
            &configuration_digest,
            &runtime,
            input,
            &context.policy,
            JevExecutionOptions {
                device: crate::JevDevice::Remote,
                dtype: crate::JevDtype::Auto,
            },
            can_start,
        )
        .await
    }

    pub(crate) async fn prepare_jev_model_request(
        &self,
        open: &winwincode_execution_port::generated::ModelOpenMessage,
        request: &mut serde_json::Value,
        can_start: &(impl Fn() -> bool + ?Sized),
    ) -> Result<(), DeviceProviderError> {
        use winwincode_execution_port::agent_config::{
            AgentSessionConfigSnapshot, validate_agent_session_config,
        };
        use winwincode_execution_port::generated::ExecutionJob;
        let session: AgentSessionConfigSnapshot = serde_json::from_value(
            request
                .get("winwincodeJevContext")
                .ok_or(DeviceProviderError::StateInvalid)?
                .clone(),
        )?;
        validate_agent_session_config(&session).map_err(|_| DeviceProviderError::StateInvalid)?;
        let task = request
            .get("winwincodeJevTask")
            .ok_or(DeviceProviderError::StateInvalid)?;
        let job: ExecutionJob = serde_json::from_value(
            task.get("job")
                .ok_or(DeviceProviderError::StateInvalid)?
                .clone(),
        )?;
        let digest = format!("sha256:{:x}", Sha256::digest(serde_json::to_vec(&job)?));
        if task.get("jobDigest").and_then(serde_json::Value::as_str) != Some(digest.as_str())
            || job.job_id != open.lease.job_id
            || request.get("provider").and_then(serde_json::Value::as_str)
                != Some(session.profile.source.settings.provider.as_str())
            || request
                .pointer("/request/model")
                .and_then(serde_json::Value::as_str)
                != Some(session.profile.source.settings.model.as_str())
        {
            return Err(DeviceProviderError::StateInvalid);
        }
        match &job.scope {
            winwincode_execution_port::generated::ExecutionScope::ProductSessionExecutionScope(
                _,
            ) => {
                if job.work_input.is_some() {
                    return Err(DeviceProviderError::StateInvalid);
                }
            }
            winwincode_execution_port::generated::ExecutionScope::WorkRunExecutionScope(_) => {
                let work_input = job
                    .work_input
                    .as_ref()
                    .ok_or(DeviceProviderError::StateInvalid)?;
                if !work_input
                    .work_plan
                    .as_ref()
                    .is_some_and(|plan| plan.iter().any(|item| item == &work_input.work_item))
                {
                    return Err(DeviceProviderError::StateInvalid);
                }
            }
        }
        self.prepare_jev_judge_request(open, &session, request, can_start)
            .await?;
        self.prepare_jev_context_items(open, &session, &job, request, can_start)
            .await?;
        let object = request
            .as_object_mut()
            .ok_or(DeviceProviderError::StateInvalid)?;
        object.remove("winwincodeJevContext");
        object.remove("winwincodeJevTask");
        Ok(())
    }

    async fn prepare_jev_context_items(
        &self,
        open: &winwincode_execution_port::generated::ModelOpenMessage,
        session: &winwincode_execution_port::agent_config::AgentSessionConfigSnapshot,
        job: &winwincode_execution_port::generated::ExecutionJob,
        request: &mut serde_json::Value,
        can_start: &(impl Fn() -> bool + ?Sized),
    ) -> Result<(), DeviceProviderError> {
        use winwincode_execution_port::jev_decision::ContextRetention;
        let Some(context) = session.profile.source.settings.jev_context.as_ref() else {
            return Ok(());
        };
        let execution_request = self.jev_execution_request(
            &format!("jev:{}:0", open.model_exchange_id.0),
            &context.provider,
            false,
        )?;
        let settings = self
            .execution_source()?
            .jev_parameters(&execution_request)?
            .settings();
        let configuration_digest = configured_context_digest(session, &settings)?;
        let task_text = context_task_text(job, request)?;
        let input = request
            .pointer_mut("/request/input")
            .and_then(serde_json::Value::as_array_mut)
            .ok_or(DeviceProviderError::StateInvalid)?;
        let last_commentary = input.iter().rposition(is_assistant_commentary);
        for index in (0..input.len()).rev() {
            if Some(index) == last_commentary || !is_assistant_commentary(&input[index]) {
                continue;
            }
            if self.model_cancelled(&open.model_exchange_id.0)? {
                return Err(DeviceProviderError::StateInvalid);
            }
            let candidate = serde_json::to_string(&input[index])?;
            let scoring_input = JevContextRequest {
                task: task_text.clone(),
                candidate,
                protected: false,
                archive_eligible: true,
            };
            let request_bytes = serde_json::to_vec(&(
                "jev-context.v1",
                &configuration_digest,
                &scoring_input,
                &context.policy,
                JevExecutionOptions {
                    device: crate::JevDevice::Remote,
                    dtype: crate::JevDtype::Auto,
                },
            ))?;
            if request_bytes.len() > MAX_JEV_REMOTE_REQUEST_BYTES {
                continue;
            }
            let result = self
                .evaluate_configured_context_once_authorized(
                    &format!("jev:{}:{index}", open.model_exchange_id.0),
                    session,
                    scoring_input,
                    can_start,
                )
                .await?;
            if let StoredJevContext::Completed { run, .. } = result
                && run.value.as_ref().is_some_and(|evaluation| {
                    matches!(
                        evaluation.decision.decision,
                        ContextRetention::Drop | ContextRetention::Archive
                    )
                })
            {
                if self.model_jev_source_item(
                    open,
                    &format!("jev:{}:{index}", open.model_exchange_id.0),
                )? != input[index]
                {
                    return Err(DeviceProviderError::StateInvalid);
                }
                input.remove(index);
            }
        }
        Ok(())
    }

    pub async fn evaluate_context_once(
        &self,
        operation_id: &str,
        configuration_digest: &str,
        runtime: &JevRuntime,
        input: JevContextRequest,
        policy: &JevPolicy,
        options: JevExecutionOptions,
    ) -> Result<StoredJevContext, DeviceProviderError> {
        self.evaluate_context_once_authorized(
            operation_id,
            configuration_digest,
            runtime,
            input,
            policy,
            options,
            &|| true,
        )
        .await
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "Exact receipt identity, scoring policy and live authorization are distinct inputs"
    )]
    async fn evaluate_context_once_authorized(
        &self,
        operation_id: &str,
        configuration_digest: &str,
        runtime: &JevRuntime,
        input: JevContextRequest,
        policy: &JevPolicy,
        options: JevExecutionOptions,
        can_start: &(impl Fn() -> bool + ?Sized),
    ) -> Result<StoredJevContext, DeviceProviderError> {
        validate_policy(policy).map_err(|_| DeviceProviderError::StateInvalid)?;
        let hash = configuration_digest
            .strip_prefix("sha256:")
            .ok_or(DeviceProviderError::StateInvalid)?;
        if operation_id.trim().is_empty()
            || operation_id.len() > 256
            || operation_id.contains(['\0', '\n', '\r'])
            || hash.len() != 64
            || !hash.bytes().all(|byte| byte.is_ascii_hexdigit())
            || input.task.trim().is_empty()
            || input.candidate.trim().is_empty()
        {
            return Err(DeviceProviderError::StateInvalid);
        }
        let request_json = serde_json::to_string(&(
            "jev-context.v1",
            configuration_digest,
            &input,
            policy,
            options,
        ))?;
        let digest = format!("{:x}", Sha256::digest(request_json.as_bytes()));
        let inserted = self.connection.execute(
            "INSERT OR IGNORE INTO jev_context_exchanges (operation_id, digest, request_json) SELECT ?1, ?2, ?3 WHERE ?4",
            params![operation_id, digest, request_json, can_start()],
        )?;
        let (original, receipt, saved_request): (String, Option<String>, Option<String>) =
            self.connection.query_row(
                "SELECT digest, result, request_json FROM jev_context_exchanges WHERE operation_id=?1",
                [operation_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            ).optional()?.ok_or(DeviceProviderError::StateInvalid)?;
        if original != digest
            || saved_request
                .as_ref()
                .is_some_and(|saved| saved != &request_json)
        {
            return Err(DeviceProviderError::StateInvalid);
        }
        if let Some(receipt) = receipt {
            return Ok(StoredJevContext::Completed {
                run: serde_json::from_str(&receipt)?,
                replayed: true,
            });
        }
        if inserted == 0 {
            return Ok(StoredJevContext::Incomplete);
        }
        let runtime = runtime.clone().with_attempt_journal(
            self.connection
                .path()
                .ok_or(DeviceProviderError::StateInvalid)?,
            operation_id,
            "context",
        );
        let run = runtime
            .evaluate_context_authorized(input, policy, options, can_start)
            .await
            .map_err(|_| DeviceProviderError::StateInvalid)?;
        let stored = self.connection.execute(
            "UPDATE jev_context_exchanges SET result=?1,call_status=?4 WHERE operation_id=?2 AND digest=?3 AND result IS NULL",
            params![serde_json::to_string(&run)?, operation_id, digest, if run.value.is_some() {"succeeded"} else {"failed"}],
        )?;
        if stored != 1 {
            return Err(DeviceProviderError::StateInvalid);
        }
        Ok(StoredJevContext::Completed {
            run: Box::new(run),
            replayed: false,
        })
    }
}

fn context_task_text(
    job: &winwincode_execution_port::generated::ExecutionJob,
    request: &serde_json::Value,
) -> Result<String, DeviceProviderError> {
    let input = request
        .pointer("/request/input")
        .and_then(serde_json::Value::as_array)
        .ok_or(DeviceProviderError::StateInvalid)?;
    let instructions: Vec<_> = input
        .iter()
        .filter(|item| {
            matches!(
                item.get("role").and_then(serde_json::Value::as_str),
                Some("system" | "developer" | "user")
            )
        })
        .collect();
    let tool_records: Vec<_> = input
        .iter()
        .filter(|item| {
            matches!(
                item.get("type").and_then(serde_json::Value::as_str),
                Some(
                    "function_call"
                        | "function_call_output"
                        | "custom_tool_call"
                        | "custom_tool_call_output"
                        | "local_shell_call"
                        | "web_search_call"
                )
            )
        })
        .collect();
    Ok(serde_json::to_string(&serde_json::json!({
        "goal": job.goal,
        "workInput": job.work_input,
        "instructions": request.pointer("/request/instructions"),
        "instructionMessages": instructions,
        "executionPlanHistory": execution_plan_history(input),
        "toolRecords": tool_records,
    }))?)
}

fn execution_plan_history(input: &[serde_json::Value]) -> Vec<&serde_json::Value> {
    let plan_calls: std::collections::BTreeSet<_> = input
        .iter()
        .filter(|item| {
            item.get("type").and_then(serde_json::Value::as_str) == Some("function_call")
                && item.get("name").and_then(serde_json::Value::as_str) == Some("update_plan")
        })
        .filter_map(|item| item.get("call_id").and_then(serde_json::Value::as_str))
        .collect();
    input
        .iter()
        .filter(|item| {
            let kind = item.get("type").and_then(serde_json::Value::as_str);
            (kind == Some("function_call_output")
                || (kind == Some("function_call")
                    && item.get("name").and_then(serde_json::Value::as_str) == Some("update_plan")))
                && item
                    .get("call_id")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|id| plan_calls.contains(id))
        })
        .collect()
}

fn is_assistant_commentary(item: &serde_json::Value) -> bool {
    item.get("type").and_then(serde_json::Value::as_str) == Some("message")
        && item.get("role").and_then(serde_json::Value::as_str) == Some("assistant")
        && item.get("phase").and_then(serde_json::Value::as_str) == Some("commentary")
        && item.as_object().is_some_and(|object| {
            object
                .keys()
                .all(|key| ["type", "role", "phase", "content", "id"].contains(&key.as_str()))
        })
        && item
            .get("content")
            .and_then(serde_json::Value::as_array)
            .is_some_and(|parts| {
                !parts.is_empty()
                    && parts.iter().all(|part| {
                        part.get("type").and_then(serde_json::Value::as_str) == Some("output_text")
                            && part
                                .get("text")
                                .and_then(serde_json::Value::as_str)
                                .is_some()
                            && part.as_object().is_some_and(|object| {
                                object
                                    .keys()
                                    .all(|key| ["type", "text"].contains(&key.as_str()))
                            })
                    })
            })
}

fn validated_system_one_settings(
    mut settings: crate::OpenJevRemoteSettings,
) -> Result<crate::OpenJevRemoteSettings, DeviceProviderError> {
    settings
        .devices
        .get_or_insert_with(|| vec!["remote".into()]);
    settings.dtypes.get_or_insert_with(|| vec!["auto".into()]);
    let (config, _) = settings
        .to_config_and_runtime()
        .map_err(|_| DeviceProviderError::StateInvalid)?;
    crate::HttpsJevRemoteTransport::try_new_system_one(&config)
        .map_err(|_| DeviceProviderError::StateInvalid)?;
    Ok(settings)
}

pub(crate) fn configured_context_digest(
    session: &winwincode_execution_port::agent_config::AgentSessionConfigSnapshot,
    settings: &crate::OpenJevRemoteSettings,
) -> Result<String, DeviceProviderError> {
    Ok(format!(
        "sha256:{:x}",
        Sha256::digest(serde_json::to_vec(&(
            "device-system-one-context.v1",
            &session.snapshot_digest,
            &settings.provider_id,
            &settings.endpoint,
            &settings.model_id,
            settings.timeout_ms,
            settings.retries,
            settings.max_batch_size,
            &settings.devices,
            &settings.dtypes,
        ))?)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        JevDevice, JevDtype, JevRemoteTransport, OpenJevRemoteProvider, RemoteJevScoreBatch,
    };
    use std::sync::Arc;
    use winwincode_execution_port::agent_config::{
        AgentProfileSettings, resolve_agent_session_config,
    };

    #[test]
    fn plan_history_preserves_outcomes_without_promoting_prose_or_unrelated_tools() {
        let input = serde_json::json!([
            {"type":"message","role":"assistant","content":"Plan updated"},
            {"type":"function_call","name":"update_plan","call_id":"p1","arguments":"invalid"},
            {"type":"function_call_output","call_id":"p1","output":"failed to parse function arguments"},
            {"type":"function_call","name":"exec_command","call_id":"exec","arguments":"echo Plan updated"},
            {"type":"function_call_output","call_id":"exec","output":"Plan updated"},
            {"type":"function_call","name":"update_plan","call_id":"p2","arguments":"{\"plan\":[]}"},
            {"type":"function_call_output","call_id":"p2","output":"Plan updated"},
            {"type":"function_call","name":"update_plan","call_id":"pending","arguments":"{}"}
        ]);
        let input = input.as_array().unwrap();
        assert_eq!(
            execution_plan_history(input),
            vec![&input[1], &input[2], &input[5], &input[6], &input[7]]
        );
        assert!(execution_plan_history(&[]).is_empty());
    }

    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "One durable operation across expiry, renewal, immutable replay and uncertain-result recovery"
    )]
    fn live_authorization_guards_each_context_operation_but_allows_exact_recovery() {
        let root = std::env::temp_dir().join(format!("wwc-context-guard-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let store = DeviceProviderStore::open(&root).unwrap();
        let provider = Arc::new(crate::MockJevProvider::healthy("context"));
        let runtime = JevRuntime::new(
            vec![provider.clone()],
            crate::JevRuntimeConfig {
                timeout: std::time::Duration::from_secs(1),
                retries: 1,
            },
        );
        let digest = format!("sha256:{}", "a".repeat(64));
        let input = JevContextRequest {
            task: "verify command".into(),
            candidate: "old commentary".into(),
            protected: false,
            archive_eligible: true,
        };
        let policy: JevPolicy = serde_json::from_value(serde_json::json!({
            "version":"fixture-policy", "minimumConfidence":0.6, "pinThreshold":0.9,
            "keepThreshold":0.9, "compactThreshold":0.9, "dropThreshold":0.9,
            "taskMemoryThreshold":0.5, "projectMemoryThreshold":0.7, "longTermMemoryThreshold":0.9
        }))
        .unwrap();
        let options = JevExecutionOptions {
            device: JevDevice::Cpu,
            dtype: JevDtype::Float32,
        };
        let executor = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        executor.block_on(async {
            let first = store
                .evaluate_context_once_authorized(
                    "jev:first",
                    &digest,
                    &runtime,
                    input.clone(),
                    &policy,
                    options,
                    &|| provider.calls() == 0,
                )
                .await
                .unwrap();
            assert!(matches!(
                first,
                StoredJevContext::Completed {
                    replayed: false,
                    ..
                }
            ));
            assert!(
                store
                    .evaluate_context_once_authorized(
                        "jev:next",
                        &digest,
                        &runtime,
                        input.clone(),
                        &policy,
                        options,
                        &|| false
                    )
                    .await
                    .is_err()
            );
            let rows: i64 = store
                .runtime.connection
                .query_row("SELECT count(*) FROM jev_context_exchanges", [], |row| {
                    row.get(0)
                })
                .unwrap();
            assert_eq!(rows, 1);
            let replay = store
                .evaluate_context_once_authorized(
                    "jev:first",
                    &digest,
                    &runtime,
                    input.clone(),
                    &policy,
                    options,
                    &|| false,
                )
                .await
                .unwrap();
            assert!(matches!(
                replay,
                StoredJevContext::Completed { replayed: true, .. }
            ));
            let changed = JevContextRequest {
                candidate: "changed".into(),
                ..input.clone()
            };
            assert!(
                store
                    .evaluate_context_once_authorized(
                        "jev:first",
                        &digest,
                        &runtime,
                        changed,
                        &policy,
                        options,
                        &|| false
                    )
                    .await
                    .is_err()
            );
            assert_eq!(provider.calls(), 1);
            store
                .evaluate_context_once_authorized(
                    "jev:next",
                    &digest,
                    &runtime,
                    input.clone(),
                    &policy,
                    options,
                    &|| true,
                )
                .await
                .unwrap();
            assert_eq!(provider.calls(), 2);
            store
                .runtime.connection
                .execute(
                    "UPDATE jev_context_exchanges SET result=NULL,call_status='pending' WHERE operation_id='jev:first'",
                    [],
                )
                .unwrap();
            assert_eq!(
                store
                    .evaluate_context_once_authorized(
                        "jev:first",
                        &digest,
                        &runtime,
                        input,
                        &policy,
                        options,
                        &|| false
                    )
                    .await
                    .unwrap(),
                StoredJevContext::Incomplete
            );
            assert_eq!(provider.calls(), 2);
        });
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[derive(Debug)]
    struct Scores(usize);

    #[test]
    fn system_one_import_defaults_are_executable_before_first_inference() {
        let root = std::env::temp_dir().join(format!("wwc-jev-defaults-{}", std::process::id()));
        let store = DeviceProviderStore::open(&root).unwrap();
        let settings: crate::OpenJevRemoteSettings = serde_json::from_value(serde_json::json!({
            "providerId":"typesafe-systemone", "endpoint":"https://nli.invalid/score",
            "apiKey":"offline-fixture", "timeoutMs":1000, "retries":0
        }))
        .unwrap();
        store.save_jev_settings(&settings).unwrap();
        let saved = store.resolve_jev_settings("typesafe-systemone").unwrap();
        let (config, _) = saved.to_config_and_runtime().unwrap();
        crate::HttpsJevRemoteTransport::try_new_system_one(&config)
            .expect("accepted Device settings must construct the actual SystemOne transport");
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn system_one_import_rejects_explicit_unsupported_hardware_before_saving() {
        let root = std::env::temp_dir().join(format!("wwc-jev-hardware-{}", std::process::id()));
        let store = DeviceProviderStore::open(&root).unwrap();
        let settings: crate::OpenJevRemoteSettings = serde_json::from_value(serde_json::json!({
            "providerId":"typesafe-systemone", "endpoint":"https://nli.invalid/score",
            "apiKey":"offline-fixture", "timeoutMs":1000, "retries":0,
            "devices":["cpu"], "dtypes":["float32"]
        }))
        .unwrap();
        assert!(store.save_jev_settings(&settings).is_err());
        assert!(store.resolve_jev_settings("typesafe-systemone").is_err());
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }

    impl JevRemoteTransport for Scores {
        fn score(
            &self,
            _: String,
            items: Vec<crate::JevHypothesis>,
            _: JevExecutionOptions,
        ) -> futures::future::BoxFuture<'static, Result<RemoteJevScoreBatch, crate::JevProviderError>>
        {
            let selected = self.0;
            Box::pin(async move {
                assert_eq!(items.len(), 4);
                Ok(RemoteJevScoreBatch {
                    evaluations: (0..4)
                        .map(|i| {
                            let entailment = if i == selected { 0.99 } else { 0.01 };
                            crate::JevScores::try_new(entailment, 1.0 - entailment, 0.0).unwrap()
                        })
                        .collect(),
                    input_tokens: 40,
                    output_tokens: Some(8),
                    resolved_model_id: Some("fixture-actual-jev".into()),
                    device: JevDevice::Remote,
                })
            })
        }
    }

    #[test]
    fn request_rebuild_replays_scores_and_preserves_every_protected_item() {
        for hypothesis in [2, 3] {
            exercise_request_rebuild(hypothesis);
        }
    }

    #[expect(
        clippy::too_many_lines,
        reason = "One durable exchange across rebuild, restart and interruption"
    )]
    fn exercise_request_rebuild(hypothesis: usize) {
        use base64::Engine as _;
        let directory =
            std::env::temp_dir().join(format!("wwc-jev-request-{}", std::process::id()));
        let store = DeviceProviderStore::open(&directory).unwrap();
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../../tests/fixtures/contracts/execution-port.valid.json"
        ))
        .unwrap();
        let messages = fixture["messages"].as_array().unwrap();
        let mut open: winwincode_execution_port::generated::ModelOpenMessage =
            serde_json::from_value(
                messages
                    .iter()
                    .find(|m| m["kind"] == "model.open")
                    .unwrap()
                    .clone(),
            )
            .unwrap();

        let mut job: winwincode_execution_port::generated::ExecutionJob = serde_json::from_value(
            messages
                .iter()
                .find(|m| m["kind"] == "job.dispatch")
                .unwrap()["job"]
                .clone(),
        )
        .unwrap();
        let work_input = job.work_input.as_mut().unwrap();
        work_input.work_plan = Some(vec![work_input.work_item.clone()]);
        let settings: crate::OpenJevRemoteSettings = serde_json::from_value(serde_json::json!({
            "providerId":"fixture-jev", "endpoint":"https://nli.invalid/score", "apiKey":"invalid-offline-key",
            "modelId":"fixture-jev-model", "timeoutMs":1000, "retries":0,
            "maxBatchSize":4, "devices":["remote"], "dtypes":["auto"]
        })).unwrap();
        store.save_jev_settings(&settings).unwrap();
        let profile: AgentProfileSettings = serde_json::from_value(serde_json::json!({
            "provider":"fixture", "model":"fixture-model", "reasoning":"max", "tools":[], "sandbox":"candidate", "instructions":null,
            "jevContext":{"provider":"fixture-jev", "policy":{"version":"fixture-policy", "minimumConfidence":0.6,
                "pinThreshold":0.9,"keepThreshold":0.9,"compactThreshold":0.9,"dropThreshold":0.9,
                "taskMemoryThreshold":0.5,"projectMemoryThreshold":0.7,"longTermMemoryThreshold":0.9}}
        })).unwrap();
        let session = resolve_agent_session_config(&winwincode_domain::WorkerId("worker-fixture".into()),
            &serde_json::from_value(serde_json::json!({"capabilityDigest":format!("sha256:{}", "a".repeat(64)), "features":[], "maxConcurrentJobs":1,"platform":"aarch64-apple-darwin"})).unwrap(),
            "executor", profile,
        ).unwrap();
        let mut original = serde_json::json!({
            "provider":"fixture", "winwincodeJevContext":session,
            "winwincodeJevTask":{"job":job, "jobDigest":format!("sha256:{:x}", Sha256::digest(serde_json::to_vec(&job).unwrap()))},
            "request":{"model":"fixture-model", "instructions":"keep instructions", "tools":[{"name":"exec_command"}], "input":[
                {"type":"message","role":"user","content":"keep goal and acceptance"},
                {"type":"message","role":"assistant","phase":"commentary","content":[{"type":"output_text","text":"obsolete progress"}]},
                {"type":"function_call","name":"update_plan","call_id":"plan","arguments":"current plan"},
                {"type":"function_call_output","call_id":"plan","output":"plan accepted"},
                {"type":"function_call_output","call_id":"exec","output":"verified tool facts"},
                {"type":"message","role":"assistant","phase":"final_answer","content":"keep result"},
                {"type":"message","role":"assistant","phase":"commentary","content":[{"type":"output_image","url":"artifact"}]},
                {"type":"message","role":"assistant","phase":"commentary","content":[{"type":"output_text","text":"current progress"}]}
            ]}
        });
        original["request"]["input"].as_array_mut().unwrap().push(
            serde_json::json!({"type":"message","role":"assistant","content":"unknown phase"}),
        );
        original["request"]["input"].as_array_mut().unwrap().extend([
            serde_json::json!({"type":"message","role":"system","content":"system constraint"}),
            serde_json::json!({"type":"message","role":"developer","content":"verifier requirement"}),
        ]);
        let payload = serde_json::to_vec(&original).unwrap();
        open.request.data_base64 = base64::engine::general_purpose::STANDARD.encode(&payload);
        open.request.payload_digest.0 = format!("sha256:{:x}", Sha256::digest(&payload));
        assert!(store.model_jev_receipts(&open).is_err());
        store.execute_model(&open).unwrap();
        assert!(store.model_jev_receipts(&open).unwrap().is_empty());
        let mut mismatched = open.clone();
        mismatched.request_id.0.push_str("-different");
        assert!(store.model_jev_receipts(&mismatched).is_err());
        let mut other_exchange = open.clone();
        other_exchange.model_exchange_id.0.push_str("-other");
        store.execute_model(&other_exchange).unwrap();

        let input = JevContextRequest {
            task: context_task_text(&job, &original).unwrap(),
            candidate: serde_json::to_string(&original["request"]["input"][1]).unwrap(),
            protected: false,
            archive_eligible: true,
        };
        let premise: serde_json::Value = serde_json::from_str(&input.task).unwrap();
        assert_eq!(premise["instructions"], "keep instructions");
        assert_eq!(
            premise["instructionMessages"],
            serde_json::json!([
                original["request"]["input"][0].clone(),
                original["request"]["input"][9].clone(),
                original["request"]["input"][10].clone(),
            ])
        );
        assert_eq!(
            premise["workInput"],
            serde_json::to_value(&job.work_input).unwrap()
        );
        assert_eq!(
            premise["workInput"]["workPlan"][0],
            premise["workInput"]["workItem"]
        );
        assert_eq!(
            premise["executionPlanHistory"],
            serde_json::json!([
                original["request"]["input"][2],
                original["request"]["input"][3],
            ])
        );
        assert_eq!(
            premise["toolRecords"],
            serde_json::json!([
                original["request"]["input"][2],
                original["request"]["input"][3],
                original["request"]["input"][4],
            ])
        );
        let (config, retry) = settings.to_config_and_runtime().unwrap();
        let runtime = JevRuntime::new(
            vec![Arc::new(OpenJevRemoteProvider::new(
                config,
                Arc::new(Scores(hypothesis)),
            ))],
            retry,
        );
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap()
            .block_on(async {
                let mut not_started = original.clone();
                assert!(store.prepare_jev_model_request(&open, &mut not_started, &|| false).await.is_err());
                let rows: i64 = store.runtime.connection.query_row("SELECT count(*) FROM jev_context_exchanges", [], |row| row.get(0)).unwrap();
                assert_eq!(rows, 0, "the real preparation entry must forward the live guard");
                let stored = store
                    .evaluate_context_once(
                        &format!("jev:{}:1", open.model_exchange_id.0),
                        &configured_context_digest(&session, &settings).unwrap(),
                        &runtime,
                        input,
                        &session
                            .profile
                            .source
                            .settings
                            .jev_context
                            .as_ref()
                            .unwrap()
                            .policy,
                        JevExecutionOptions {
                            device: JevDevice::Remote,
                            dtype: JevDtype::Auto,
                        },
                    )
                    .await
                    .unwrap();
                let StoredJevContext::Completed { run, .. } = stored else {
                    panic!("fixture receipt")
                };
                assert_eq!(
                    run.value.as_ref().unwrap().decision.decision,
                    if hypothesis == 2 {
                        winwincode_execution_port::jev_decision::ContextRetention::Archive
                    } else {
                        winwincode_execution_port::jev_decision::ContextRetention::Drop
                    }
                );
                assert_eq!(run.observation.unwrap().input_tokens, 40);
                let (saved_digest, saved_request): (String, String) = store.runtime.connection.query_row(
                    "SELECT digest, request_json FROM jev_context_exchanges WHERE operation_id=?1",
                    [format!("jev:{}:1", open.model_exchange_id.0)],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                ).unwrap();
                assert_eq!(saved_digest, format!("{:x}", Sha256::digest(saved_request.as_bytes())));
                let saved: serde_json::Value = serde_json::from_str(&saved_request).unwrap();
                assert_eq!(saved[2]["task"], context_task_text(&job, &original).unwrap());
                assert_eq!(saved[2]["candidate"], serde_json::to_string(&original["request"]["input"][1]).unwrap());
                assert!(!saved_request.contains("invalid-offline-key"));

                let mut expected = original.clone();
                expected["request"]["input"]
                    .as_array_mut()
                    .unwrap()
                    .remove(1);
                expected
                    .as_object_mut()
                    .unwrap()
                    .remove("winwincodeJevContext");
                expected
                    .as_object_mut()
                    .unwrap()
                    .remove("winwincodeJevTask");
                let mut prepared = original.clone();
                store
                    .prepare_jev_model_request(&open, &mut prepared, &|| false)
                    .await
                    .unwrap();
                assert_eq!(prepared, expected);
                let mut oversized = original.clone();
                oversized["request"]["input"][1]["content"][0]["text"] =
                    serde_json::json!("x".repeat(MAX_JEV_REMOTE_REQUEST_BYTES));
                let mut unchanged = oversized.clone();
                unchanged.as_object_mut().unwrap().remove("winwincodeJevContext");
                unchanged.as_object_mut().unwrap().remove("winwincodeJevTask");
                store.prepare_jev_model_request(&open, &mut oversized, &|| true).await.unwrap();
                assert_eq!(oversized, unchanged);
                assert_eq!(store.runtime.connection.query_row(
                    "SELECT COUNT(*) FROM jev_context_exchanges", [], |row| row.get::<_, i64>(0)
                ).unwrap(), 1);
                for omit in [true, false] {
                    let mut invalid_job = job.clone();
                    let input = invalid_job.work_input.as_mut().unwrap();
                    if omit {
                        input.work_plan = None;
                    } else {
                        input.work_plan.as_mut().unwrap()[0].goal.push_str(" changed");
                    }
                    let mut changed = original.clone();
                    changed["winwincodeJevTask"] = serde_json::json!({
                        "jobDigest":format!("sha256:{:x}", Sha256::digest(serde_json::to_vec(&invalid_job).unwrap())),
                        "job":invalid_job,
                    });
                    assert!(store.prepare_jev_model_request(&open, &mut changed, &|| true).await.is_err());
                }
                for pointer in ["/request/instructions", "/request/input/0/content"] {
                    let mut changed = original.clone();
                    *changed.pointer_mut(pointer).unwrap() = serde_json::json!("new requirement");
                    assert!(
                        store
                            .prepare_jev_model_request(&open, &mut changed, &|| true)
                            .await
                            .is_err()
                    );
                }
                let receipts = store.model_jev_receipts(&open).unwrap();
                assert_eq!(receipts.len(), 1);
                assert_eq!(
                    store.model_jev_source_item(&open, &receipts[0].operation_id).unwrap(),
                    original["request"]["input"][1]
                );
                assert!(store.model_jev_source_item(&other_exchange, &receipts[0].operation_id).is_err());
                assert!(store.model_jev_source_item(&open, "jev:foreign:1").is_err());
                assert!(
                    store
                        .model_jev_receipts(&other_exchange)
                        .unwrap()
                        .is_empty()
                );
                assert_eq!(
                    receipts[0].operation_id,
                    format!("jev:{}:1", open.model_exchange_id.0)
                );
                assert_eq!(
                    receipts[0]
                        .run
                        .as_ref()
                        .unwrap()
                        .observation
                        .as_ref()
                        .unwrap()
                        .input_tokens,
                    40
                );
                assert_eq!(
                    receipts[0]
                        .run
                        .as_ref()
                        .unwrap()
                        .observation
                        .as_ref()
                        .unwrap()
                        .output_tokens,
                    Some(8)
                );

                let mut foreign_score = saved.clone();
                foreign_score[2]["candidate"] = serde_json::json!("{\"foreign\":true}");
                let foreign_score = serde_json::to_string(&foreign_score).unwrap();
                store.runtime.connection.execute(
                    "UPDATE jev_context_exchanges SET request_json=?1,digest=?2",
                    params![foreign_score, format!("{:x}", Sha256::digest(foreign_score.as_bytes()))],
                ).unwrap();
                assert!(store.model_jev_source_item(&open, &receipts[0].operation_id).is_err());
                assert!(store.runtime.connection.execute("UPDATE jev_context_exchanges SET request_json=NULL", []).is_err());
                assert!(store.model_jev_source_item(&open, &receipts[0].operation_id).is_err());
                store.runtime.connection.execute(
                    "UPDATE jev_context_exchanges SET request_json=?1,digest=?2",
                    params![saved_request, saved_digest],
                ).unwrap();
                let reopened = DeviceProviderStore::open(&directory).unwrap();
                let mut replay = original.clone();
                reopened
                    .prepare_jev_model_request(&open, &mut replay, &|| true)
                    .await
                    .unwrap();
                assert_eq!(replay, expected);
                assert_eq!(reopened.model_jev_receipts(&open).unwrap(), receipts);
                reopened
                    .runtime.connection
                    .execute("UPDATE jev_context_exchanges SET result=NULL,call_status='pending'", [])
                    .unwrap();
                let pending = reopened.model_jev_receipts(&open).unwrap();
                assert_eq!(pending.len(), 1);
                assert!(pending[0].run.is_none());
                let mut interrupted = original.clone();
                reopened
                    .prepare_jev_model_request(&open, &mut interrupted, &|| true)
                    .await
                    .unwrap();
                let mut retained = original.clone();
                retained
                    .as_object_mut()
                    .unwrap()
                    .remove("winwincodeJevContext");
                retained
                    .as_object_mut()
                    .unwrap()
                    .remove("winwincodeJevTask");
                assert_eq!(interrupted, retained);
                reopened.runtime.connection.execute("UPDATE jev_context_exchanges SET request_json='tampered'", []).unwrap();
                assert!(reopened.model_jev_receipts(&open).is_err());
                let mut corrupt_replay = original.clone();
                assert!(reopened.prepare_jev_model_request(&open, &mut corrupt_replay, &|| true).await.is_err());
                assert_eq!(corrupt_replay, original);


                let mut altered = original.clone();
                altered["winwincodeJevTask"]["job"]["goal"] = serde_json::json!("tampered");
                assert!(
                    reopened
                        .prepare_jev_model_request(&open, &mut altered, &|| true)
                        .await
                        .is_err()
                );
            });
        let count: i64 = store
            .runtime
            .connection
            .query_row("SELECT count(*) FROM jev_context_exchanges", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(count, 1);
        drop(store);
        std::fs::remove_dir_all(directory).unwrap();
    }
}
