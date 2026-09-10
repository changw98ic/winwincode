// SPDX-License-Identifier: Apache-2.0

//! Immutable dispatch-payload rotation for scheduler-owned Worker replacement.

use serde_json::{Number, Value};
use sha2::{Digest, Sha256};
use winwincode_domain::{Sha256Digest, WorkRunId, is_canonical_prefixed_id};

use crate::{ExecutionJobRecord, StorageError};

const MAX_EXECUTION_ATTEMPT: u64 = 1_000;
const HEX: &[u8; 16] = b"0123456789ABCDEF";

pub(crate) struct ReplacementDispatchPayload {
    pub attempt: u64,
    pub work_run_id: Option<WorkRunId>,
    pub bytes: Vec<u8>,
}

pub(crate) fn replacement_dispatch_payload(
    job: &ExecutionJobRecord,
) -> Result<ReplacementDispatchPayload, StorageError> {
    let attempt = job
        .attempt
        .checked_add(1)
        .filter(|attempt| *attempt <= MAX_EXECUTION_ATTEMPT)
        .ok_or_else(|| StorageError::invalid_input("execution replacement attempt is exhausted"))?;
    let mut decoded: Value = serde_json::from_slice(&job.dispatch_payload).map_err(|_| {
        StorageError::adapter("execution replacement dispatch payload is not valid JSON")
    })?;
    let object = decoded.as_object_mut().ok_or_else(|| {
        StorageError::adapter("execution replacement dispatch payload is not an object")
    })?;
    let stored_attempt = object
        .get("attempt")
        .and_then(Value::as_u64)
        .ok_or_else(|| {
            StorageError::adapter("execution replacement dispatch attempt is missing")
        })?;
    if stored_attempt != job.attempt {
        return Err(StorageError::adapter(
            "execution replacement dispatch attempt differs from its queue record",
        ));
    }
    object.insert("attempt".to_owned(), Value::Number(Number::from(attempt)));
    let predecessor_work_run = object
        .get("scope")
        .and_then(Value::as_object)
        .and_then(|scope| scope.get("workRunId"))
        .map(|value| {
            value.as_str().map(str::to_owned).ok_or_else(|| {
                StorageError::adapter("execution replacement WorkRunId must be a string")
            })
        })
        .transpose()?;
    if predecessor_work_run.as_deref() != job.work_run_id.as_ref().map(|id| id.0.as_str()) {
        return Err(StorageError::adapter(
            "execution replacement WorkRunId does not match queue record",
        ));
    }
    let work_run_id = predecessor_work_run
        .map(|value| {
            if !is_canonical_prefixed_id(&value, "wrn_") {
                return Err(StorageError::adapter(
                    "execution replacement WorkRunId is not canonical",
                ));
            }
            let successor = successor_work_run_id(&job.job_id, attempt);
            object
                .get_mut("scope")
                .and_then(Value::as_object_mut)
                .expect("scope object")
                .insert("attempt".to_owned(), Value::Number(Number::from(attempt)));
            object
                .get_mut("scope")
                .and_then(Value::as_object_mut)
                .expect("scope object")
                .insert("workRunId".to_owned(), Value::String(successor.clone()));
            Ok(WorkRunId(successor))
        })
        .transpose()?;
    if job.work_run_id.is_some() && work_run_id.is_none() {
        return Err(StorageError::adapter(
            "execution replacement WorkRunId is missing",
        ));
    }
    let bytes = serde_json::to_vec(&decoded).map_err(|_| {
        StorageError::adapter("execution replacement dispatch payload cannot be encoded")
    })?;
    Ok(ReplacementDispatchPayload {
        attempt,
        work_run_id,
        bytes,
    })
}

pub(crate) fn logical_dispatch_digest(
    dispatch_payload: &[u8],
) -> Result<Sha256Digest, StorageError> {
    let mut decoded: Value = serde_json::from_slice(dispatch_payload).map_err(|_| {
        StorageError::adapter("execution replacement dispatch payload is not valid JSON")
    })?;
    let object = decoded.as_object_mut().ok_or_else(|| {
        StorageError::adapter("execution replacement dispatch payload is not an object")
    })?;
    if object.remove("attempt").is_none() {
        return Err(StorageError::adapter(
            "execution replacement dispatch attempt is missing",
        ));
    }
    if let Some(scope) = object.get_mut("scope").and_then(Value::as_object_mut) {
        scope.remove("attempt");
        scope.remove("workRunId");
    }
    let encoded = serde_json::to_vec(&decoded).map_err(|_| {
        StorageError::adapter("logical execution replacement payload cannot be encoded")
    })?;
    Ok(Sha256Digest(format!(
        "sha256:{:x}",
        Sha256::digest(encoded)
    )))
}

fn successor_work_run_id(job_id: &winwincode_domain::ExecutionJobId, attempt: u64) -> String {
    let digest = Sha256::digest(format!(
        "winwincode.work-run-replacement\0{}\0{attempt}",
        job_id.0
    ));
    let mut suffix = String::with_capacity(26);
    for byte in &digest[..13] {
        suffix.push(char::from(HEX[(byte >> 4) as usize]));
        suffix.push(char::from(HEX[(byte & 0x0F) as usize]));
    }
    format!("wrn_{suffix}")
}

#[cfg(test)]
mod tests {
    use winwincode_domain::{
        ExecutionJobId, Instant, OrganizationId, ProductSessionId, ProjectId, RepositoryId,
        RequestId, Sha256Digest, WorkspaceId,
    };

    use super::*;
    use crate::{ExecutionJobState, ExecutionQueueScope};

    fn record(payload: &Value) -> ExecutionJobRecord {
        let mut record =
            record_with_attempt(payload, payload["attempt"].as_u64().expect("attempt"));
        record.work_run_id = payload["scope"]["workRunId"]
            .as_str()
            .map(|value| WorkRunId(value.to_owned()));
        record
    }

    fn record_with_attempt(payload: &Value, attempt: u64) -> ExecutionJobRecord {
        ExecutionJobRecord {
            scope: ExecutionQueueScope {
                organization_id: OrganizationId("org_00000000000000000000000001".into()),
                workspace_id: WorkspaceId("wsp_00000000000000000000000002".into()),
                project_id: ProjectId("prj_00000000000000000000000003".into()),
                repository_id: RepositoryId("rep_00000000000000000000000004".into()),
                product_session_id: ProductSessionId("psn_00000000000000000000000005".into()),
                delivery_id: None,
            },
            job_id: ExecutionJobId("job_00000000000000000000000006".into()),
            submission_request_id: RequestId("req_00000000000000000000000007".into()),
            payload_digest: Sha256Digest(format!("sha256:{}", "a".repeat(64))),
            dispatch_payload: serde_json::to_vec(payload).expect("payload"),
            state: ExecutionJobState::Running,
            attempt,
            revision: 3,
            dependencies: Vec::new(),
            work_run_id: None,
            submitted_at: Instant("2027-10-01T10:00:01.000Z".into()),
            updated_at: Instant("2027-10-01T10:00:04.000Z".into()),
            cancellation: None,
        }
    }

    #[test]
    fn replacement_rotates_work_run_and_attempt() {
        let original = serde_json::json!({
            "attempt": 1,
            "executionProfile": "local-codex",
            "goal": "keep every immutable field",
            "jobId": "job_00000000000000000000000006",
            "payloadDigest": format!("sha256:{}", "a".repeat(64)),
            "scope": { "workRunId": "wrn_00000000000000000000000001" },
        });
        let replacement =
            replacement_dispatch_payload(&record(&original)).expect("replacement payload");
        assert_eq!(replacement.attempt, 2);
        let mut expected = original;
        expected["attempt"] = Value::from(2);
        expected["scope"]["attempt"] = Value::from(2);
        expected["scope"]["workRunId"] = Value::from(successor_work_run_id(
            &ExecutionJobId("job_00000000000000000000000006".into()),
            2,
        ));
        assert_eq!(
            replacement.work_run_id,
            Some(WorkRunId(successor_work_run_id(
                &ExecutionJobId("job_00000000000000000000000006".into()),
                2
            )))
        );
        assert_eq!(
            serde_json::from_slice::<Value>(&replacement.bytes).expect("decode"),
            expected
        );
    }

    #[test]
    fn replacement_rejects_a_payload_with_foreign_attempt_authority() {
        let error = replacement_dispatch_payload(&record_with_attempt(
            &serde_json::json!({"attempt": 2}),
            1,
        ))
        .err()
        .expect("foreign attempt must fail");
        assert_eq!(error.kind(), crate::StorageErrorKind::Adapter);
    }

    #[test]
    fn replacement_work_run_identity_changes_across_attempts() {
        let first = serde_json::json!({
            "attempt": 1,
            "scope": { "workRunId": "wrn_01J00000000000000000000000" }
        });
        let first_payload =
            replacement_dispatch_payload(&record(&first)).expect("first replacement");
        assert_eq!(
            first_payload.work_run_id,
            Some(WorkRunId(successor_work_run_id(
                &ExecutionJobId("job_00000000000000000000000006".into()),
                2
            )))
        );
        let first_id = &first_payload.work_run_id.as_ref().expect("first WorkRun").0;
        assert!(is_canonical_prefixed_id(first_id, "wrn_"));
        assert_ne!(
            first_id,
            first["scope"]["workRunId"]
                .as_str()
                .expect("original WorkRun")
        );
        let second_record =
            record(&serde_json::from_slice(&first_payload.bytes).expect("first bytes"));
        let second_payload =
            replacement_dispatch_payload(&second_record).expect("second replacement");
        assert_ne!(first_payload.work_run_id, second_payload.work_run_id);
        assert!(is_canonical_prefixed_id(
            &second_payload
                .work_run_id
                .as_ref()
                .expect("second WorkRun")
                .0,
            "wrn_"
        ));
        assert_eq!(
            second_payload.work_run_id,
            Some(WorkRunId(successor_work_run_id(
                &ExecutionJobId("job_00000000000000000000000006".into()),
                3
            )))
        );
    }

    #[test]
    fn replacement_rejects_malformed_or_mismatched_work_run_without_panicking() {
        for value in ["wrn_", "wrn_短", "run_00000000000000000000000001"] {
            let payload = serde_json::json!({"attempt": 1, "scope": {"workRunId": value}});
            assert!(replacement_dispatch_payload(&record(&payload)).is_err());
        }
        for value in [serde_json::json!(7), Value::Null] {
            let payload = serde_json::json!({"attempt": 1, "scope": {"workRunId": value}});
            assert!(replacement_dispatch_payload(&record(&payload)).is_err());
        }
        let payload = serde_json::json!({
            "attempt": 1,
            "scope": {"workRunId": "wrn_01J00000000000000000000000"}
        });
        let mut job = record(&payload);
        job.work_run_id = Some(WorkRunId("wrn_01J00000000000000000000001".into()));
        assert!(replacement_dispatch_payload(&job).is_err());
        let payload = serde_json::json!({
            "attempt": 1,
            "scope": {"workRunId": "wrn_01J00000000000000000000000"}
        });
        let job_without_work_run = record_with_attempt(&payload, 1);
        assert!(replacement_dispatch_payload(&job_without_work_run).is_err());
    }

    #[test]
    fn successor_ids_are_distinct_across_jobs() {
        let first =
            successor_work_run_id(&ExecutionJobId("job_00000000000000000000000006".into()), 2);
        let second =
            successor_work_run_id(&ExecutionJobId("job_00000000000000000000000007".into()), 2);
        assert_ne!(first, second);
    }
}
