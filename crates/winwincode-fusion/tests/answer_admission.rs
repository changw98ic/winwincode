// SPDX-License-Identifier: Apache-2.0

use serde_json::{Value, json};
use winwincode_fusion::answer_from_frames;

fn answer_frames(phase: Option<Value>, end_turn: Option<Value>) -> Vec<String> {
    let mut item = json!({"type":"message","role":"assistant",
        "content":[{"type":"output_text","text":"{\"claims\":[]}"}]});
    if let Some(phase) = phase {
        item["phase"] = phase;
    }
    let mut terminal = json!({"type":"completed","responseId":"fixture-response"});
    if let Some(end_turn) = end_turn {
        terminal["endTurn"] = end_turn;
    }
    vec![
        json!({"type":"created"}).to_string(),
        json!({"type":"output_item_done","item":item}).to_string(),
        terminal.to_string(),
    ]
}

#[test]
fn fusion_answer_admission_accepts_missing_optional_phase_and_end_turn() {
    assert_eq!(
        answer_from_frames(&answer_frames(None, None)),
        Some(json!({"claims":[]}))
    );
}

#[test]
fn fusion_answer_admission_accepts_explicit_null_optional_phase_and_end_turn() {
    assert_eq!(
        answer_from_frames(&answer_frames(Some(Value::Null), Some(Value::Null))),
        Some(json!({"claims":[]}))
    );
}

#[test]
fn fusion_answer_admission_accepts_explicit_final_answer_and_true_terminal() {
    assert_eq!(
        answer_from_frames(&answer_frames(
            Some(json!("final_answer")),
            Some(json!(true))
        )),
        Some(json!({"claims":[]}))
    );
}

#[test]
fn fusion_answer_admission_rejects_nonfinal_turn_and_commentary() {
    for frames in [
        answer_frames(Some(json!("final_answer")), Some(json!(false))),
        answer_frames(Some(json!("commentary")), Some(json!(true))),
    ] {
        assert_eq!(answer_from_frames(&frames), None);
    }
}

#[test]
fn fusion_answer_admission_rejects_missing_error_and_duplicate_terminal() {
    let complete = answer_frames(Some(json!("final_answer")), Some(json!(true)));
    let mut incomplete = complete.clone();
    incomplete.pop();
    let mut failed = complete.clone();
    failed.insert(
        1,
        json!({"type":"error","error":{"code":"FIXTURE_FAILURE"}}).to_string(),
    );
    let mut duplicate = complete.clone();
    duplicate.push(complete.last().unwrap().clone());
    for frames in [incomplete, failed, duplicate] {
        assert_eq!(answer_from_frames(&frames), None);
    }
}

#[test]
fn fusion_answer_admission_rejects_function_or_custom_tool_requests_even_with_final_text() {
    for item in [
        json!({"type":"function_call","call_id":"fixture-call","name":"fixture","arguments":"{}"}),
        json!({"type":"custom_tool_call","call_id":"fixture-call","name":"fixture","input":"fixture"}),
    ] {
        let mut frames = answer_frames(Some(json!("final_answer")), Some(json!(true)));
        frames.insert(
            1,
            json!({"type":"output_item_done","item":item}).to_string(),
        );
        assert_eq!(answer_from_frames(&frames), None);
    }
}

#[test]
fn fusion_answer_admission_rejects_invalid_json_and_nonobject_answers() {
    for text in ["{", "[]", "null", "42", "\"fixture\""] {
        let mut frames = answer_frames(Some(json!("final_answer")), Some(json!(true)));
        let mut done: Value = serde_json::from_str(&frames[1]).unwrap();
        done["item"]["content"][0]["text"] = json!(text);
        frames[1] = done.to_string();
        assert_eq!(answer_from_frames(&frames), None);
    }
}

#[test]
fn fusion_answer_admission_rejects_added_tools_and_tool_input_deltas() {
    for frame in [
        json!({"type":"output_item_added","item":{"type":"function_call","call_id":"fixture-call","name":"fixture","arguments":""}}),
        json!({"type":"output_item_added","item":{"type":"custom_tool_call","call_id":"fixture-call","name":"fixture","input":""}}),
        json!({"type":"tool_call_input_delta","callId":"fixture-call","itemId":"fixture-item","delta":"fixture"}),
    ] {
        let mut frames = answer_frames(Some(json!("final_answer")), Some(json!(true)));
        frames.insert(1, frame.to_string());
        assert_eq!(answer_from_frames(&frames), None);
    }
}

#[test]
fn fusion_answer_admission_rejects_invalid_optional_field_types() {
    for value in [json!("true"), json!(1), json!({}), json!([])] {
        assert_eq!(
            answer_from_frames(&answer_frames(
                Some(json!("final_answer")),
                Some(value.clone())
            )),
            None
        );
        assert_eq!(
            answer_from_frames(&answer_frames(Some(value), Some(json!(true)))),
            None
        );
    }
}

#[test]
fn fusion_answer_admission_rejects_duplicate_final_messages() {
    let mut frames = answer_frames(Some(json!("final_answer")), Some(json!(true)));
    frames.insert(2, frames[1].clone());
    assert_eq!(answer_from_frames(&frames), None);
}
