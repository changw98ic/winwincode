// SPDX-License-Identifier: Apache-2.0

//! Benchmark-only admission at the last model-stream boundary before Core
//! dispatches a complete tool request. The ledger contains digests, not inputs.

use rusqlite::{OptionalExtension as _, TransactionBehavior, params};
use serde_json::Value;
use sha2::{Digest as _, Sha256};

use crate::store::{AdapterStore, AdapterStoreError};

pub(crate) const STOP_REASON: &str = "STUCK_TOOL_REPEAT_LIMIT";

impl AdapterStore {
    pub(crate) fn enable_tool_repeat_guard(&self, run: &str) -> Result<(), AdapterStoreError> {
        self.lock()?
            .execute(
                "INSERT OR IGNORE INTO tool_repeat_run(run_key) VALUES (?1)",
                [run],
            )
            .map_err(|_| AdapterStoreError::Unavailable)?;
        Ok(())
    }

    pub(crate) fn tool_repeat_stopped(&self, run: &str) -> Result<bool, AdapterStoreError> {
        self.lock()?
            .query_row(
                "SELECT stopped FROM tool_repeat_run WHERE run_key = ?1",
                [run],
                |row| row.get(0),
            )
            .optional()
            .map(|stopped| stopped.unwrap_or(false))
            .map_err(|_| AdapterStoreError::Unavailable)
    }

    /// Returns false once the sixth occurrence is committed. Exact replay of
    /// an admitted call does not count twice; changing that call is a conflict.
    pub(crate) fn admit_tool_output(
        &self,
        run: &str,
        model_call: &str,
        output: &str,
    ) -> Result<bool, AdapterStoreError> {
        let mut connection = self.lock()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|_| AdapterStoreError::Unavailable)?;
        let stopped: Option<bool> = transaction
            .query_row(
                "SELECT stopped FROM tool_repeat_run WHERE run_key = ?1",
                [run],
                |row| row.get(0),
            )
            .optional()
            .map_err(|_| AdapterStoreError::Unavailable)?;
        match stopped {
            None => return Ok(true),
            Some(true) => return Ok(false),
            Some(false) => {}
        }
        let Some((call_id, digest)) = tool_identity(output)? else {
            return Ok(true);
        };
        let existing: Option<String> = transaction
            .query_row(
                "SELECT request_digest FROM tool_repeat_admission
                 WHERE run_key = ?1 AND model_call_id = ?2 AND call_id = ?3",
                params![run, model_call, call_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(|_| AdapterStoreError::Unavailable)?;
        if let Some(existing) = existing {
            return if existing == digest {
                Ok(true)
            } else {
                Err(AdapterStoreError::Conflict)
            };
        }
        transaction
            .execute(
                "INSERT INTO tool_repeat_admission(run_key, model_call_id, call_id, request_digest)
             VALUES (?1, ?2, ?3, ?4)",
                params![run, model_call, call_id, digest],
            )
            .map_err(|_| AdapterStoreError::Unavailable)?;
        let count: i64 = transaction.query_row(
            "SELECT COUNT(*) FROM tool_repeat_admission WHERE run_key = ?1 AND request_digest = ?2",
            params![run, digest], |row| row.get(0),
        ).map_err(|_| AdapterStoreError::Unavailable)?;
        if count >= 6 {
            transaction
                .execute(
                    "UPDATE tool_repeat_run SET stopped = 1 WHERE run_key = ?1",
                    [run],
                )
                .map_err(|_| AdapterStoreError::Unavailable)?;
        }
        transaction
            .commit()
            .map_err(|_| AdapterStoreError::Unavailable)?;
        Ok(count < 6)
    }
}

fn tool_identity(output: &str) -> Result<Option<(String, String)>, AdapterStoreError> {
    let frame: Value = serde_json::from_str(output).map_err(|_| AdapterStoreError::Corrupt)?;
    if frame["type"] != "output_item_done" {
        return Ok(None);
    }
    let item = &frame["item"];
    let kind = item["type"].as_str().ok_or(AdapterStoreError::Corrupt)?;
    let args = match kind {
        "function_call" => {
            let arguments = item["arguments"]
                .as_str()
                .ok_or(AdapterStoreError::Corrupt)?;
            serde_json::from_str(arguments).unwrap_or_else(|_| Value::String(arguments.to_owned()))
        }
        "custom_tool_call" => Value::String(
            item["input"]
                .as_str()
                .ok_or(AdapterStoreError::Corrupt)?
                .to_owned(),
        ),
        "local_shell_call" => item
            .get("action")
            .cloned()
            .ok_or(AdapterStoreError::Corrupt)?,
        "tool_search_call" => item
            .get("arguments")
            .cloned()
            .ok_or(AdapterStoreError::Corrupt)?,
        _ => return Ok(None),
    };
    let call_id = item["call_id"]
        .as_str()
        .or_else(|| item["id"].as_str())
        .filter(|id| !id.is_empty())
        .ok_or(AdapterStoreError::Corrupt)?;
    let name = if matches!(kind, "function_call" | "custom_tool_call") {
        item["name"]
            .as_str()
            .filter(|name| !name.is_empty())
            .ok_or(AdapterStoreError::Corrupt)?
    } else {
        kind
    };
    let namespace = item["namespace"]
        .as_str()
        .filter(|name| !name.is_empty())
        .unwrap_or("functions");
    // Complete arguments include target paths, workdir and requested content.
    // Custom input is preserved byte-for-byte, so distinct patches stay distinct.
    let identity = canonical_args(serde_json::json!({
        "type": kind, "namespace": namespace, "name": name, "args": args,
        "execution": item.get("execution"),
    }));
    let bytes = serde_json::to_vec(&identity).map_err(|_| AdapterStoreError::Corrupt)?;
    Ok(Some((
        call_id.to_owned(),
        format!("{:x}", Sha256::digest(bytes)),
    )))
}

fn canonical_args(value: Value) -> Value {
    match value {
        Value::Array(items) => Value::Array(items.into_iter().map(canonical_args).collect()),
        Value::Object(items) => {
            let mut items = items
                .into_iter()
                .filter(|(key, _)| {
                    let key: String = key
                        .chars()
                        .filter(char::is_ascii_alphanumeric)
                        .map(|ch| ch.to_ascii_lowercase())
                        .collect();
                    !matches!(
                        key.as_str(),
                        "requestid" | "timestamp" | "timestampms" | "progress" | "progressmetadata"
                    )
                })
                .collect::<Vec<_>>();
            items.sort_by(|a, b| a.0.cmp(&b.0));
            Value::Object(
                items
                    .into_iter()
                    .map(|(key, value)| (key, canonical_args(value)))
                    .collect(),
            )
        }
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(call: usize, args: &Value) -> String {
        serde_json::json!({"type":"output_item_done", "item": {
            "type":"function_call", "call_id":format!("call-{call}"),
            "name":"exec_command", "arguments":args.to_string(),
        }})
        .to_string()
    }

    #[test]
    fn repeat_admission_survives_restart_replay_and_child_model_calls() {
        let root = std::env::temp_dir().join(format!("wwc-tool-repeat-{}", uuid::Uuid::new_v4()));
        let store = AdapterStore::open(&root).unwrap();
        // Ordinary runs have no implicit benchmark cap.
        for call in 0..7 {
            assert!(
                store
                    .admit_tool_output(
                        "ordinary",
                        "model",
                        &request(call, &serde_json::json!({"cmd":"ls"}))
                    )
                    .unwrap()
            );
        }
        store.enable_tool_repeat_guard("run").unwrap();
        for call in 0..5 {
            let output = request(call, &serde_json::json!({"cmd":"ls", "request_id":call}));
            assert!(
                store
                    .admit_tool_output("run", "root-model", &output)
                    .unwrap()
            );
            assert!(
                store
                    .admit_tool_output("run", "root-model", &output)
                    .unwrap()
            );
        }
        assert!(matches!(
            store.admit_tool_output(
                "run",
                "root-model",
                &request(0, &serde_json::json!({"cmd":"pwd"}))
            ),
            Err(AdapterStoreError::Conflict)
        ));
        drop(store);
        let store = AdapterStore::open(&root).unwrap();
        assert!(
            !store
                .admit_tool_output(
                    "run",
                    "child-model",
                    &request(0, &serde_json::json!({"timestamp":99,"cmd":"ls"}))
                )
                .unwrap()
        );
        assert!(store.tool_repeat_stopped("run").unwrap());
        drop(store);
        let store = AdapterStore::open(&root).unwrap();
        assert!(store.tool_repeat_stopped("run").unwrap());
        assert!(
            !store
                .admit_tool_output(
                    "run",
                    "later",
                    &request(9, &serde_json::json!({"cmd":"pwd"}))
                )
                .unwrap()
        );
        store.enable_tool_repeat_guard("other-run").unwrap();
        assert!(
            store
                .admit_tool_output(
                    "other-run",
                    "model",
                    &request(0, &serde_json::json!({"cmd":"ls"}))
                )
                .unwrap()
        );
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn identity_preserves_content_target_and_namespace() {
        let custom = |input| {
            serde_json::json!({"type":"output_item_done","item":{
                "type":"custom_tool_call","name":"apply_patch","call_id":"patch","input":input,
            }})
            .to_string()
        };
        assert_ne!(
            tool_identity(&custom("update x: first")).unwrap(),
            tool_identity(&custom("update x: second")).unwrap()
        );
        assert_ne!(
            tool_identity(&request(0, &serde_json::json!({"cmd":"ls","workdir":"/a"}))).unwrap(),
            tool_identity(&request(0, &serde_json::json!({"cmd":"ls","workdir":"/b"}))).unwrap()
        );
        let a = request(0, &serde_json::json!({"cmd":"ls"}));
        let mut b: Value = serde_json::from_str(&a).unwrap();
        b["item"]["namespace"] = Value::String("other".into());
        assert_ne!(
            tool_identity(&a).unwrap(),
            tool_identity(&b.to_string()).unwrap()
        );
        assert_eq!(
            tool_identity(r#"{"type":"output_item_added"}"#).unwrap(),
            None
        );
        let mut malformed: Value = serde_json::from_str(&a).unwrap();
        malformed["item"]["arguments"] = Value::String("{unfinished".into());
        assert!(tool_identity(&malformed.to_string()).unwrap().is_some());
    }
}
