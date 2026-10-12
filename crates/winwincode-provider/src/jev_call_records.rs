// SPDX-License-Identifier: Apache-2.0

use super::{
    JevAttemptFailure, JevObservation, JevProvider, JevProviderCapabilities, JevProviderError,
    JevProviderErrorKind, JevRun, JevRuntime,
};
use crate::DeviceProviderError;
use rusqlite::{Connection, params};

pub(super) struct Invocation {
    connection: Connection,
    operation: String,
    sequence: i64,
}

impl JevRuntime {
    pub(super) async fn invoke<T>(
        &self,
        provider: std::sync::Arc<dyn JevProvider>,
        operation: &impl Fn(
            std::sync::Arc<dyn JevProvider>,
        ) -> futures::future::BoxFuture<'static, Result<T, JevProviderError>>,
    ) -> Option<Result<T, JevProviderError>> {
        let inference = Box::pin(tokio::time::timeout(
            self.config.timeout,
            operation(provider),
        ));
        let result = if let Some(cancel) = &self.cancellation {
            match futures::future::select(inference, Box::pin(cancel.cancelled())).await {
                futures::future::Either::Left((result, _)) => result,
                futures::future::Either::Right(_) => return None,
            }
        } else {
            inference.await
        };
        Some(result.unwrap_or_else(|_| Err(JevProviderError::new(JevProviderErrorKind::Timeout))))
    }

    pub(super) fn cancelled_invocation<T>(
        &self,
        invocation: Option<&Invocation>,
        failures: Vec<JevAttemptFailure>,
        provider: &str,
    ) -> JevRun<T> {
        if Self::finish_invocation(
            invocation,
            "cancelled",
            &serde_json::json!({"status":"cancelled","effects":"unknown"}),
        )
        .is_err()
        {
            return Self::storage_failed_run(failures, provider);
        }
        self.stopped_run(failures, provider, winwincode_network::Acceptance::Unknown)
    }

    pub(super) fn begin_invocation(
        &self,
        capabilities: &JevProviderCapabilities,
        request: &serde_json::Value,
    ) -> Result<Option<Invocation>, DeviceProviderError> {
        let Some((path, operation, _role)) = &self.attempt_journal else {
            return Ok(None);
        };
        let mut connection = Connection::open(path)?;
        connection.busy_timeout(std::time::Duration::from_secs(5))?;
        let transaction =
            connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let sequence: i64 = transaction.query_row(
            "SELECT COALESCE(MAX(sequence),0)+1 FROM jev_invocation_attempts WHERE operation_id=?1",
            [operation],
            |row| row.get(0),
        )?;
        let request = serde_json::json!({"providerId":capabilities.provider_id,"modelId":capabilities.model_id,"request":request}).to_string();
        transaction.execute("INSERT INTO jev_invocation_attempts(operation_id,sequence,request_json) VALUES(?1,?2,?3)",params![operation,sequence,request])?;
        transaction.commit()?;
        Ok(Some(Invocation {
            connection,
            operation: operation.clone(),
            sequence,
        }))
    }

    pub(super) fn finish_invocation(
        invocation: Option<&Invocation>,
        status: &str,
        result: &serde_json::Value,
    ) -> Result<(), DeviceProviderError> {
        if let Some(invocation) = invocation {
            let changed = invocation.connection.execute("UPDATE jev_invocation_attempts SET status=?1,result_json=?2 WHERE operation_id=?3 AND sequence=?4 AND status='pending'",
                params![status,result.to_string(),invocation.operation,invocation.sequence])?;
            if changed != 1 {
                return Err(DeviceProviderError::StateInvalid);
            }
        }
        Ok(())
    }

    pub(super) fn successful_run<T: serde::Serialize>(
        invocation: Option<&Invocation>,
        value: T,
        observation: JevObservation,
        failures: Vec<JevAttemptFailure>,
        provider: &str,
    ) -> JevRun<T> {
        if Self::finish_invocation(
            invocation,
            "succeeded",
            &serde_json::json!({"value":&value,"observation":&observation}),
        )
        .is_err()
        {
            return Self::storage_failed_run(failures, provider);
        }
        JevRun {
            value: Some(value),
            observation: Some(observation),
            failures,
        }
    }

    pub(super) fn storage_failed_run<T>(
        mut failures: Vec<JevAttemptFailure>,
        provider: &str,
    ) -> JevRun<T> {
        failures.push(JevAttemptFailure {
            provider_id: provider.into(),
            kind: JevProviderErrorKind::Unavailable,
            latency: std::time::Duration::ZERO,
            network: Some(winwincode_network::NetworkFailure::new(
                winwincode_network::ErrorKind::StorageUnavailable,
                winwincode_network::Acceptance::Unknown,
                winwincode_network::Phase::Persist,
            )),
            attempt: None,
            connection_wait: false,
        });
        JevRun::without_decision(failures)
    }
}
