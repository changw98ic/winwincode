// SPDX-License-Identifier: Apache-2.0
use super::{MAX_SAFE_INTEGER, portable_value, safe_public_text};
use winwincode_domain::{CoreToolCallReference, CoreToolRuntimeProjection, ExecutionSequence};

pub(super) fn valid(meta: &CoreToolRuntimeProjection) -> bool {
    portable_value(&meta.source_thread_id, 200)
        && sequence(&meta.source_sequence)
        && meta.call.as_ref().is_none_or(|call| {
            sequence(&call.request_sequence)
                && call.parent_request_sequence.as_ref().is_none_or(sequence)
                && portable_value(&call.logical_id, 200)
                && portable_value(&call.tool_name, 500)
                && optional(call.parent_call_id.as_deref())
                && optional(call.cell_id.as_deref())
                && optional(call.attempt_id.as_deref())
        })
        && meta.cell.as_ref().is_none_or(|cell| {
            sequence(&cell.sequence)
                && sequence(&cell.parent_request_sequence)
                && portable_value(&cell.cell_id, 200)
        })
        && meta.wait.as_ref().is_none_or(|wait| {
            sequence(&wait.request_sequence) && sequence(&wait.target_cell_sequence)
        })
        && meta.sharing.as_ref().is_none_or(|sharing| {
            sequence(&sharing.source_request_sequence)
                && portable_value(&sharing.source_attempt_id, 200)
        })
        && meta.diagnosis.as_ref().is_none_or(|diagnosis| {
            portable_value(&diagnosis.diagnostic_id, 200)
                && sequence(&diagnosis.evidence_version)
                && diagnosis
                    .question
                    .as_deref()
                    .is_none_or(|question| safe_public_text(question, 4096))
                && diagnosis.evidence.len() <= 8
                && diagnosis.evidence.iter().all(reference)
        })
        && meta.recovery.as_ref().is_none_or(|recovery| {
            sequence(&recovery.request_sequence)
                && recovery
                    .business_id
                    .as_deref()
                    .is_none_or(|id| safe_public_text(id, 500))
        })
}
fn optional(value: Option<&str>) -> bool {
    value.is_none_or(|value| portable_value(value, 200))
}
fn sequence(sequence: &ExecutionSequence) -> bool {
    u64::try_from(sequence.0).is_ok_and(|value| value > 0 && value <= MAX_SAFE_INTEGER)
}
fn reference(call: &CoreToolCallReference) -> bool {
    sequence(&call.request_sequence)
        && portable_value(&call.logical_id, 200)
        && portable_value(&call.tool_name, 500)
        && optional(call.parent_call_id.as_deref())
        && optional(call.cell_id.as_deref())
}
