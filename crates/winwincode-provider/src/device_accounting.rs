// SPDX-License-Identifier: Apache-2.0

//! Source-bound financial statements exported independently of runtime replay.

use crate::{DeviceProviderError, DeviceProviderStore};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use rusqlite::params;
use sha2::{Digest, Sha256};
use winwincode_domain::Sha256Digest;
use winwincode_execution_port::{
    accounting::{AttemptAccountingStatement, ProviderAccountingReceipt},
    action_enforcement::ActionEnforcementSigningKey,
    generated::{ExecutionLeaseStamp, ModelChunkMessage, ModelOpenMessage},
};

impl DeviceProviderStore {
    /// Freezes a Server-confirmed finished/fenced attempt and exports only durable
    /// Provider receipts. Missing response or price facts remain unknown.
    /// # Errors
    /// Rejects changed identities, corrupt sources, and unavailable storage.
    #[allow(clippy::too_many_lines)]
    pub fn accounting_statement(
        &self,
        lease: &ExecutionLeaseStamp,
        key: &ActionEnforcementSigningKey,
    ) -> Result<Option<AttemptAccountingStatement>, DeviceProviderError> {
        // Receipt scans and validation must not reserve the Device's writer
        // while another task is starting a call or retaining a paid response.
        // A changed WAL snapshot refuses the final upgrade and retries accounting.
        self.connection.execute_batch("BEGIN DEFERRED")?;
        let result = (|| {
            let mut query = self.connection.prepare("SELECT digest,request_open,COALESCE(accounting_chunks,chunks) FROM exchanges WHERE request_open IS NOT NULL AND json_extract(request_open,'$.lease.jobId')=?1 AND json_extract(request_open,'$.lease.attempt')=?2 ORDER BY exchange_id")?;
            let rows = query
                .query_map(params![lease.job_id.0, lease.attempt], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Option<String>>(2)?,
                    ))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            if rows.is_empty() || rows.iter().any(|(_, _, chunks)| chunks.is_none()) {
                return Ok(None);
            }
            let mut statement = AttemptAccountingStatement {
                lease: lease.clone(),
                manifest: Vec::new(),
                receipts: Vec::new(),
                signature: Sha256Digest(String::new()),
            };
            for (digest, request, chunks) in rows {
                if digest != format!("{:x}", Sha256::digest(request.as_bytes())) {
                    return Err(DeviceProviderError);
                }
                let open: ModelOpenMessage = serde_json::from_str(&request)?;
                if open.lease.job_id != lease.job_id
                    || open.lease.lease_id != lease.lease_id
                    || open.lease.attempt != lease.attempt
                    || open.lease.worker_id != lease.worker_id
                    || open.lease.worker_instance_id != lease.worker_instance_id
                    || open.lease.fencing_token != lease.fencing_token
                    || open.lease.issued_at != lease.issued_at
                {
                    return Err(DeviceProviderError);
                }
                let payload: serde_json::Value =
                    serde_json::from_slice(&crate::device_model::validated_model_payload(&open)?)?;
                let provider = payload
                    .get("provider")
                    .and_then(serde_json::Value::as_str)
                    .ok_or(DeviceProviderError)?;
                let slot = format!("primary:{}", open.model_exchange_id.0);
                statement.manifest.push(slot.clone());
                let chunks: Vec<ModelChunkMessage> =
                    serde_json::from_str(chunks.as_deref().ok_or(DeviceProviderError)?)?;
                if let Some(chunk) = chunks.last().filter(|chunk| chunk.is_final) {
                    if chunk.model_exchange_id != open.model_exchange_id
                        || chunk.lease.job_id != lease.job_id
                        || chunk.lease.lease_id != lease.lease_id
                        || chunk.lease.fencing_token != lease.fencing_token
                        || chunk.lease.worker_id != lease.worker_id
                        || chunk.lease.worker_instance_id != lease.worker_instance_id
                        || chunk.lease.attempt != lease.attempt
                        || chunk.lease.issued_at != lease.issued_at
                        || chunk.worker_session_id != open.worker_session_id
                        || chunk.session_identity != open.session_identity
                    {
                        return Err(DeviceProviderError);
                    }
                    if let Some(payload) = &chunk.payload {
                        let bytes = STANDARD
                            .decode(&payload.data_base64)
                            .map_err(|_| DeviceProviderError)?;
                        if payload.payload_digest.0
                            != format!("sha256:{:x}", Sha256::digest(&bytes))
                        {
                            return Err(DeviceProviderError);
                        }
                        let value: serde_json::Value = serde_json::from_slice(&bytes)?;
                        let usage = value.get("tokenUsage").or_else(|| value.get("token_usage"));
                        let tokens = usage
                            .filter(|usage| !usage.is_null())
                            .and_then(|usage| {
                                metric(usage, "inputTokens", "input_tokens").zip(metric(
                                    usage,
                                    "outputTokens",
                                    "output_tokens",
                                ))
                            })
                            .map(|(input, output)| {
                                input.checked_add(output).ok_or(DeviceProviderError)
                            })
                            .transpose()?;
                        let cost_microunits =
                            metric(&value, "actualCostMicros", "actual_cost_micros");
                        if tokens.is_some() || cost_microunits.is_some() {
                            let response_id = value
                                .get("responseId")
                                .or_else(|| value.get("response_id"))
                                .and_then(serde_json::Value::as_str)
                                .unwrap_or(&open.model_exchange_id.0);
                            statement.receipts.push(ProviderAccountingReceipt {
                                slot,
                                model_exchange_id: open.model_exchange_id.clone(),
                                provider_id: provider.to_owned(),
                                provider_receipt_id: response_id.to_owned(),
                                source_digest: Sha256Digest(format!(
                                    "sha256:{:x}",
                                    Sha256::digest(&bytes)
                                )),
                                tokens,
                                cost_microunits,
                            });
                        }
                    }
                }
                for receipt in self.model_jev_receipts(&open)? {
                    append_jev(&mut statement, &open, &receipt)?;
                }
                for receipt in self.model_jev_judge_receipts(&open)? {
                    append_jev(&mut statement, &open, &receipt)?;
                }
            }
            statement.manifest.sort();
            statement.receipts.sort_by(|a, b| a.slot.cmp(&b.slot));
            statement.sign(key).map_err(|_| DeviceProviderError)?;
            // Freeze only the fully checked snapshot. Keep its write transaction
            // limited to this insert and commit; failed upgrades export nothing.
            self.connection.execute(
                "INSERT OR IGNORE INTO accounting_closed_attempts VALUES(?1,?2,?3)",
                params![lease.job_id.0, lease.attempt, lease.lease_id.0],
            )?;
            Ok(Some(statement))
        })();
        if result.is_ok() {
            self.connection.execute_batch("COMMIT")?;
        } else {
            let _ = self.connection.execute_batch("ROLLBACK");
        }
        result
    }
}

fn metric(value: &serde_json::Value, camel: &str, snake: &str) -> Option<u64> {
    value
        .get(camel)
        .or_else(|| value.get(snake))
        .and_then(serde_json::Value::as_u64)
}

fn append_jev<T: serde::Serialize>(
    statement: &mut AttemptAccountingStatement,
    open: &ModelOpenMessage,
    receipt: &crate::DeviceJevReceipt<T>,
) -> Result<(), DeviceProviderError> {
    statement.manifest.push(receipt.operation_id.clone());
    if let Some(run) = &receipt.run {
        for (index, _) in run.failures.iter().enumerate() {
            statement
                .manifest
                .push(format!("{}:failed:{index}", receipt.operation_id));
        }
        if let Some(observation) = &run.observation {
            let bytes = serde_json::to_vec(receipt)?;
            statement.receipts.push(ProviderAccountingReceipt {
                slot: receipt.operation_id.clone(),
                model_exchange_id: open.model_exchange_id.clone(),
                provider_id: observation.provider_id.clone(),
                provider_receipt_id: receipt.operation_id.clone(),
                source_digest: Sha256Digest(format!("sha256:{:x}", Sha256::digest(bytes))),
                tokens: observation
                    .output_tokens
                    .and_then(|output| observation.input_tokens.checked_add(output)),
                cost_microunits: None,
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use winwincode_execution_port::generated::EncodedPayload;

    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "one cancelled source is checked across export, restart, and forbidden re-execution"
    )]
    fn cancelled_receipts_survive_restart_and_closed_attempts_cannot_open_more_calls() {
        let root = std::env::temp_dir().join(format!(
            "device-accounting-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../../tests/fixtures/contracts/execution-port.valid.json"
        ))
        .unwrap();
        let mut open: ModelOpenMessage = serde_json::from_value(
            fixture["messages"]
                .as_array()
                .unwrap()
                .iter()
                .find(|message| message["kind"] == "model.open")
                .unwrap()
                .clone(),
        )
        .unwrap();
        let request = serde_json::to_vec(
            &serde_json::json!({"provider":"fixture-provider","request":{"model":"fixture-model"}}),
        )
        .unwrap();
        open.request = EncodedPayload {
            content_type: "application/json".into(),
            data_base64: STANDARD.encode(&request),
            payload_digest: Sha256Digest(format!("sha256:{:x}", Sha256::digest(&request))),
        };
        let key = ActionEnforcementSigningKey::from_bytes([7; 32]).unwrap();
        let store = DeviceProviderStore::open(&root).unwrap();
        let saved = serde_json::to_string(&open).unwrap();
        store
            .connection
            .execute(
                "INSERT INTO exchanges(exchange_id,digest,request_open) VALUES(?1,?2,?3)",
                params![
                    open.model_exchange_id.0,
                    format!("{:x}", Sha256::digest(saved.as_bytes())),
                    saved
                ],
            )
            .unwrap();
        assert!(
            store
                .accounting_statement(&open.lease, &key)
                .unwrap()
                .is_none(),
            "running source cannot close an attempt"
        );
        let payload=serde_json::to_vec(&serde_json::json!({"type":"completed","responseId":"provider-receipt-1","tokenUsage":{"inputTokens":85,"outputTokens":20},"actualCostMicros":47})).unwrap();
        let mut chunk = crate::model_failure(&open, "fixture cancelled after payment");
        chunk.payload = Some(EncodedPayload {
            content_type: "application/json".into(),
            data_base64: STANDARD.encode(&payload),
            payload_digest: Sha256Digest(format!("sha256:{:x}", Sha256::digest(&payload))),
        });
        let price_only = serde_json::to_vec(
            &serde_json::json!({"response_id":"provider-receipt-1","actual_cost_micros":47}),
        )
        .unwrap();
        let mut first = chunk.clone();
        first.payload = Some(EncodedPayload {
            content_type: "application/json".into(),
            data_base64: STANDARD.encode(&price_only),
            payload_digest: Sha256Digest(format!("sha256:{:x}", Sha256::digest(&price_only))),
        });
        store
            .connection
            .execute(
                "UPDATE exchanges SET cancelled=1,accounting_chunks=?1 WHERE exchange_id=?2",
                params![
                    serde_json::to_string(&vec![first]).unwrap(),
                    open.model_exchange_id.0
                ],
            )
            .unwrap();
        let price_statement = store
            .accounting_statement(&open.lease, &key)
            .unwrap()
            .unwrap();
        assert_eq!(
            (
                price_statement.receipts[0].tokens,
                price_statement.receipts[0].cost_microunits
            ),
            (None, Some(47))
        );
        store
            .connection
            .execute(
                "UPDATE exchanges SET cancelled=1,accounting_chunks=?1 WHERE exchange_id=?2",
                params![
                    serde_json::to_string(&vec![chunk]).unwrap(),
                    open.model_exchange_id.0
                ],
            )
            .unwrap();
        let statement = store
            .accounting_statement(&open.lease, &key)
            .unwrap()
            .unwrap();
        statement.verify(&key).unwrap();
        assert_eq!(statement.manifest, price_statement.manifest);
        assert_eq!(
            (
                statement.receipts[0].tokens,
                statement.receipts[0].cost_microunits
            ),
            (Some(105), Some(47))
        );
        assert!(
            store
                .replay_model(&open.model_exchange_id.0, 1)
                .unwrap()
                .is_empty()
        );
        drop(store);
        let store = DeviceProviderStore::open(&root).unwrap();
        assert_eq!(
            store.accounting_statement(&open.lease, &key).unwrap(),
            Some(statement)
        );
        let mut foreign = open.clone();
        foreign.lease.fencing_token.0 = "999".into();
        assert!(store.accounting_statement(&foreign.lease, &key).is_err());
        let mut extra = open.clone();
        extra.model_exchange_id.0 = "mdl_00000000000000000000000099".into();
        assert!(
            store.execute_model(&extra).is_err(),
            "closed attempt rejects before any paid effect"
        );
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }
}
