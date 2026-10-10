// SPDX-License-Identifier: Apache-2.0
use super::ContextualUserFragment;
use super::ToolReceipt;
use super::ToolReceipts;
use codex_state::ToolExecutionStatus;
use codex_state::ToolOutputDelivery;
use codex_state::ToolOutputDisposition;
use codex_utils_output_truncation::approx_token_count;

#[test]
fn receipts_keep_bounded_metadata_and_drop_oversized_identifiers() {
    let receipts = (0..12)
        .map(|index| ToolReceipt {
            source_id: "x".repeat(if index == 0 { 81 } else { 80 }),
            tool: "😀".repeat(64),
            request_sequence: index,
            execution: Some(ToolExecutionStatus::Completed),
            disposition: ToolOutputDisposition::Accepted,
            delivery: ToolOutputDelivery::Offered,
            source_request_sequence: Some(42),
        })
        .collect();
    let fragment = ToolReceipts(receipts);
    let body: serde_json::Value = serde_json::from_str(&fragment.body()).unwrap();
    let references = body["receipts"].as_array().unwrap();
    assert_eq!(references.len(), 4);
    assert_eq!(references[0]["request_sequence"], 1);
    assert_eq!(references[3]["request_sequence"], 4);
    assert!(approx_token_count(&fragment.render()) < 1_000);
}
