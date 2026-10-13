// SPDX-License-Identifier: Apache-2.0

//! Required private request/result records. Recovery is scoped to a confirmed
//! terminated Worker instance; opening a shared provider store never fails live work.

use rusqlite::params;
use winwincode_execution_port::generated::ModelOpenMessage;

use crate::{DeviceProviderError, DeviceProviderStore};

impl DeviceProviderStore {
    pub(crate) fn retain_cancelled_model_open(
        &self,
        open: &ModelOpenMessage,
    ) -> Result<(), DeviceProviderError> {
        use sha2::{Digest as _, Sha256};
        let transaction = rusqlite::Transaction::new_unchecked(
            &self.connection,
            rusqlite::TransactionBehavior::Immediate,
        )?;
        let request = serde_json::to_string(open)?;
        let digest = format!("{:x}", Sha256::digest(request.as_bytes()));
        self.connection.execute("UPDATE exchanges SET request_open=?1,digest=?2 WHERE exchange_id=?3 AND request_open IS NULL AND cancelled=1",params![request,digest,open.model_exchange_id.0])?;
        let saved: String = self.connection.query_row(
            "SELECT request_open FROM exchanges WHERE exchange_id=?1",
            [&open.model_exchange_id.0],
            |row| row.get(0),
        )?;
        if saved != request {
            return Err(DeviceProviderError::StateInvalid);
        }
        let pending: bool = self.connection.query_row(
            "SELECT chunks IS NULL FROM exchanges WHERE exchange_id=?1",
            [&open.model_exchange_id.0],
            |row| row.get(0),
        )?;
        if pending {
            self.persist_model_response(
                &open.model_exchange_id.0,
                &[crate::device_model::model_failure(
                    open,
                    "DEVICE_MODEL_CANCELLED",
                )],
            )?;
        }
        transaction.commit()?;
        Ok(())
    }

    /// Closes pending calls belonging to a Worker whose process termination has
    /// been verified by its supervisor. Never call this for a live or unknown boot.
    ///
    /// # Errors
    /// Returns an error if an invocation still owns its OS lock or storage fails.
    pub fn fail_terminated_worker_calls(
        &self,
        worker_instance_id: &str,
    ) -> Result<usize, DeviceProviderError> {
        let requests = {
            let mut query = self.connection.prepare(
                "SELECT request_open FROM exchanges WHERE request_open IS NOT NULL
                 AND json_extract(request_open,'$.lease.workerInstanceId')=?1",
            )?;
            query
                .query_map([worker_instance_id], |row| row.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?
        };
        let mut closed = 0;
        for request in requests {
            let open: ModelOpenMessage = serde_json::from_str(&request)?;
            let _owner = self
                .try_model_exchange_owner(&open.model_exchange_id.0)?
                .ok_or(DeviceProviderError::StateInvalid)?;
            let transaction = rusqlite::Transaction::new_unchecked(
                &self.connection,
                rusqlite::TransactionBehavior::Immediate,
            )?;
            self.settle_interrupted_invocations(&open)?;
            let failure = vec![crate::device_model::model_failure(
                &open,
                "DEVICE_MODEL_INTERRUPTED",
            )];
            let pending: bool = self.connection.query_row(
                "SELECT chunks IS NULL FROM exchanges WHERE exchange_id=?1",
                [&open.model_exchange_id.0],
                |row| row.get(0),
            )?;
            if pending {
                self.persist_model_response(&open.model_exchange_id.0, &failure)?;
                closed += 1;
            }
            for prefix in ["jev", "judge"] {
                let operation_prefix = format!("{prefix}:{}:", open.model_exchange_id.0);
                let table = if prefix == "jev" {
                    "jev_context_exchanges"
                } else {
                    "jev_judge_exchanges"
                };
                let operation_ids=self.connection.prepare(&format!("SELECT operation_id FROM {table} WHERE result IS NULL AND substr(operation_id,1,length(?1))=?1"))?
                    .query_map([&operation_prefix],|row|row.get::<_,String>(0))?.collect::<Result<Vec<_>,_>>()?;
                for operation in operation_ids {
                    let result = interrupted_jev_result(&self.connection, &operation)?;
                    self.connection.execute(&format!("UPDATE {table} SET result=?1,call_status='failed' WHERE operation_id=?2 AND result IS NULL"),params![result,operation])?;
                }
                let failure=serde_json::json!({"status":"failed","code":"jev_execution_interrupted","network":winwincode_network::NetworkFailure::new(
                    winwincode_network::ErrorKind::TransportInterrupted,winwincode_network::Acceptance::Unknown,winwincode_network::Phase::Stream)}).to_string();
                self.connection.execute("UPDATE jev_invocation_attempts SET status='failed',result_json=?1 WHERE status='pending' AND substr(operation_id,1,length(?2))=?2",params![failure,operation_prefix])?;
            }
            self.connection.execute(
                "DELETE FROM model_retry_state WHERE exchange_id=?1",
                [&open.model_exchange_id.0],
            )?;
            transaction.commit()?;
        }
        Ok(closed)
    }
}

fn interrupted_jev_result(
    connection: &rusqlite::Connection,
    operation: &str,
) -> Result<String, DeviceProviderError> {
    let attempts=connection.prepare("SELECT request_json,status,result_json FROM jev_invocation_attempts WHERE operation_id=?1 ORDER BY sequence")?
        .query_map([operation],|row|Ok((row.get::<_,String>(0)?,row.get::<_,String>(1)?,row.get::<_,Option<String>>(2)?)))?.collect::<Result<Vec<_>,_>>()?;
    let mut observation = None;
    let mut failures = Vec::new();
    for (request, status, result) in attempts {
        let request: serde_json::Value = serde_json::from_str(&request)?;
        let provider = request["providerId"]
            .as_str()
            .unwrap_or("unknown")
            .to_owned();
        let result: Option<serde_json::Value> =
            result.as_deref().map(serde_json::from_str).transpose()?;
        if status == "succeeded" {
            observation = result
                .as_ref()
                .and_then(|value| value.get("observation"))
                .cloned();
        } else {
            let network = result
                .as_ref()
                .and_then(|value| value.get("network"))
                .cloned()
                .unwrap_or_else(|| {
                    serde_json::json!(winwincode_network::NetworkFailure::new(
                        winwincode_network::ErrorKind::TransportInterrupted,
                        winwincode_network::Acceptance::Unknown,
                        winwincode_network::Phase::Stream
                    ))
                });
            failures.push(serde_json::json!({"providerId":provider,"kind":"unavailable","latency":{"secs":0,"nanos":0},"network":network,"attempt":null,"connectionWait":false}));
        }
    }
    // A logical failure does not erase an already measured physical response.
    // With no physical receipt, retain unknown acceptance rather than zero usage.
    if failures.is_empty() && observation.is_none() {
        failures.push(serde_json::json!({"providerId":"unknown","kind":"unavailable","latency":{"secs":0,"nanos":0},"network":winwincode_network::NetworkFailure::new(
            winwincode_network::ErrorKind::TransportInterrupted,winwincode_network::Acceptance::Unknown,winwincode_network::Phase::Stream),"attempt":null,"connectionWait":false}));
    }
    Ok(serde_json::json!({"value":null,"observation":observation,"failures":failures}).to_string())
}
