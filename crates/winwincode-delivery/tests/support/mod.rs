// SPDX-License-Identifier: Apache-2.0

use std::collections::HashMap;

use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

/// One-time test-boundary migration for the frozen TypeScript oracle.
///
/// The committed oracle intentionally remains byte-for-byte unchanged. This
/// function is not linked into the Delivery crate and does not make canonical
/// serde accept legacy fields. A deterministic migration namespace supplies
/// identities that did not exist in DSH; it never claims they were observed
/// Worker facts.
pub fn migrate_legacy_typescript_snapshot(mut snapshot: Value) -> Result<Value, String> {
    let delivery_id = required_string(&snapshot, "id")?.to_owned();
    let runs = snapshot
        .get("stageRuns")
        .and_then(Value::as_array)
        .ok_or_else(|| "legacy snapshot stageRuns must be an array".to_owned())?;
    let mut runs_by_id = HashMap::new();
    for run in runs {
        let id = required_string(run, "id")?;
        runs_by_id.insert(id.to_owned(), run.clone());
    }

    let bindings = snapshot
        .get_mut("sessionBindings")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| "legacy snapshot sessionBindings must be an array".to_owned())?;
    let mut migrated = Vec::with_capacity(bindings.len());
    for binding in bindings.iter() {
        if [
            "productSessionId",
            "executionJobId",
            "workerSessionId",
            "codexThreadId",
        ]
        .iter()
        .any(|field| binding.get(*field).is_some())
        {
            return Err("legacy migration rejects mixed SessionBinding shapes".to_owned());
        }
        let stage_run_id = required_string(binding, "stageRunId")?;
        let run = runs_by_id
            .get(stage_run_id)
            .ok_or_else(|| format!("legacy binding references missing StageRun {stage_run_id}"))?;
        if required_string(run, "actorType")? == "human" {
            // Human review is a Control Plane ProductSession action in the new
            // model, not an ExecutionJob-backed SessionBinding.
            continue;
        }
        let dsh_session_id = binding
            .get("dshSessionId")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                format!("Codex StageRun {stage_run_id} lacks its legacy DSH identity")
            })?;
        let codex_session_id = binding
            .get("codexSessionId")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                format!("Codex StageRun {stage_run_id} lacks its legacy Codex identity")
            })?;
        let binding_id = required_string(binding, "id")?;
        let mut value = Map::new();
        value.insert(
            "schemaVersion".to_owned(),
            binding
                .get("schemaVersion")
                .cloned()
                .ok_or_else(|| "legacy binding lacks schemaVersion".to_owned())?,
        );
        value.insert("id".to_owned(), Value::String(binding_id.to_owned()));
        value.insert("deliveryId".to_owned(), Value::String(delivery_id.clone()));
        value.insert(
            "deliveryTaskId".to_owned(),
            run.get("deliveryTaskId").cloned().unwrap_or(Value::Null),
        );
        value.insert(
            "stageRunId".to_owned(),
            Value::String(stage_run_id.to_owned()),
        );
        value.insert(
            "productSessionId".to_owned(),
            Value::String(migration_id("psn_", dsh_session_id)),
        );
        value.insert(
            "executionJobId".to_owned(),
            Value::String(migration_id(
                "job_",
                &format!("{delivery_id}:{stage_run_id}:{binding_id}"),
            )),
        );
        value.insert(
            "workerSessionId".to_owned(),
            Value::String(migration_id("wsn_", dsh_session_id)),
        );
        value.insert(
            "codexThreadId".to_owned(),
            Value::String(migration_id("cdx_", codex_session_id)),
        );
        value.insert(
            "boundAtMillis".to_owned(),
            binding
                .get("boundAtMillis")
                .cloned()
                .ok_or_else(|| "legacy binding lacks boundAtMillis".to_owned())?,
        );
        migrated.push(Value::Object(value));
    }
    *bindings = migrated;
    Ok(snapshot)
}

fn required_string<'value>(value: &'value Value, field: &str) -> Result<&'value str, String> {
    value
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("legacy value lacks string field {field}"))
}

fn migration_id(prefix: &str, input: &str) -> String {
    // Uppercase hexadecimal is a deterministic subset of the canonical
    // Crockford Base32 identifier alphabet.
    let digest = format!("{:X}", Sha256::digest(input.as_bytes()));
    format!("{prefix}{}", &digest[..26])
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{migrate_legacy_typescript_snapshot, migration_id};

    #[test]
    fn deterministic_migration_identity_uses_the_canonical_identifier_alphabet() {
        let id = migration_id("job_", "legacy-delivery:run:binding");

        assert_eq!(id.len(), 30);
        assert!(id.starts_with("job_"));
        assert!(
            id[4..]
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'A'..=b'F'))
        );
        assert_eq!(id, migration_id("job_", "legacy-delivery:run:binding"));
    }

    #[test]
    fn mixed_binding_shape_fails_closed() {
        let value = json!({
            "id": "delivery",
            "stageRuns": [{
                "id": "run",
                "actorType": "codex",
                "deliveryTaskId": null
            }],
            "sessionBindings": [{
                "schemaVersion": 3,
                "id": "binding",
                "stageRunId": "run",
                "dshSessionId": "legacy-dsh",
                "codexSessionId": "legacy-codex",
                "productSessionId": "already-new",
                "boundAtMillis": 1
            }]
        });
        assert!(migrate_legacy_typescript_snapshot(value).is_err());
    }
}
