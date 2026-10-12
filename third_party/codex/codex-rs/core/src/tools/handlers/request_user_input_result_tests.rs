// SPDX-License-Identifier: Apache-2.0

use super::*;
#[test]
fn unicode_and_escaped_control_answers_reconstruct_exactly_across_bounded_pages() {
    let request = codex_state::ToolRequestIdentity {
        thread_id: "thread".into(),
        logical_id: "input".into(),
        turn_id: "turn".into(),
        scope_id: "scope".into(),
        cell_id: None,
        parent_call_id: None,
        tool_name: "request_user_input".into(),
        source: "code_mode".into(),
        binding: "digest".into(),
    };
    let fact = codex_state::ToolExecutionFact {
        schema_version: 1,
        request_sequence: 1,
        request,
        resolution: codex_state::ToolRequestResolution::Observed,
        attempt: None,
    };
    let answer = "汉🦀\n\u{0000}\"\\".repeat(10_000);
    let response_json = json!({"answers":{"same":{"answers":[answer]}}}).to_string();
    let record = ControlInputRecovery {
        result_ref: "input-result:ref".into(),
        parent: fact.clone(),
        input: fact,
        response_digest: digest(json!(response_json)),
        response_json,
    };
    let mut supplied = "0".to_owned();
    let mut reconstructed = String::new();
    let mut pages = 0;
    loop {
        let encoded = page(&record, &supplied).unwrap();
        assert!(encoded.len() <= MAX_PAGE_BYTES);
        let decoded: Value = serde_json::from_str(&encoded).unwrap();
        reconstructed.push_str(decoded["content"].as_str().unwrap());
        pages += 1;
        match decoded["nextCursor"].as_str() {
            Some(next) => supplied = next.into(),
            None => break,
        }
    }
    assert!(pages > 2);
    assert_eq!(reconstructed, record.response_json);
    assert_eq!(digest(json!(reconstructed)), record.response_digest);
    assert!(page(&record, "1:forged").is_err());
    assert!(page(&record, &cursor(&record, usize::MAX)).is_err());
}
