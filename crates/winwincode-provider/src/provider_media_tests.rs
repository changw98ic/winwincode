// SPDX-License-Identifier: Apache-2.0

use super::*;
use crate::provider_anthropic::AnthropicCodecDiagnostic;
use crate::provider_anthropic::prepare_anthropic_request;

fn payload(output: &Value) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "requestId":"req-1", "provider":"fixture", "sessionId":"session-1", "threadId":"thread-1",
        "request": {
            "model":"fixture", "instructions":"", "tool_choice":"auto", "parallel_tool_calls":true,
            "stream":true, "store":false,
            "tools":[
                {"type":"custom", "name":"exec", "description":"run", "format":{"type":"grammar","syntax":"lark","definition":"start: /.+/"}},
                {"type":"function", "name":"wait", "parameters":{"type":"object","properties":{}}}
            ],
            "input":[
                {"type":"message", "role":"user", "content":[{"type":"input_text","text":"inspect"}]},
                {"type":"custom_tool_call", "name":"exec", "call_id":"exec-1", "input":"image(await tools.capture({}));"},
                {"type":"custom_tool_call_output", "call_id":"exec-1", "output":output},
                {"type":"function_call", "name":"wait", "call_id":"wait-1", "arguments":"{}"},
                {"type":"function_call_output", "call_id":"wait-1", "output":output}
            ]
        }
    })).unwrap()
}

#[test]
fn mixed_images_keep_their_outer_call_identity_after_exec_and_wait() {
    let output = json!([
        {"type":"input_text", "text":"image before yield"},
        {"type":"input_image", "image_url":"data:image/png;base64,iVBORw0KGgo=", "detail":"original"},
        {"type":"input_text", "text":"image after yield"}
    ]);
    let options = AnthropicMessagesOptions {
        max_output_tokens: 4096,
        pricing: ProviderTokenPricing::default(),
    };
    let bytes = payload(&output);
    let openai: Value = serde_json::from_slice(
        &prepare_openai_chat_request(&bytes, "fixture", options)
            .unwrap()
            .body,
    )
    .unwrap();
    let anthropic: Value = serde_json::from_slice(
        &prepare_anthropic_request(&bytes, "fixture", options)
            .unwrap()
            .body,
    )
    .unwrap();
    let anthropic_results = anthropic["messages"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|message| message["content"].as_array().unwrap())
        .filter(|block| block["type"] == "tool_result")
        .cloned()
        .collect::<Vec<_>>();
    let mut expected = Vec::new();
    let mut expected_openai = vec![json!({"role":"user", "content":"inspect"})];
    for call_id in ["exec-1", "wait-1"] {
        let marker = format!("Tool call source_id: {call_id}");
        expected.push(json!({"type":"tool_result", "tool_use_id":call_id, "content":[
            {"type":"text","text":marker},
            {"type":"text","text":"image before yield"},
            {"type":"image","source":{"type":"base64","media_type":"image/png","data":"iVBORw0KGgo="}},
            {"type":"text","text":"image after yield"}
        ]}));
        expected_openai.push(if call_id == "exec-1" {
            json!({"role":"assistant", "content":null, "tool_calls":[{
                "id":call_id, "type":"function", "function":{"name":"exec","arguments":"{\"input\":\"image(await tools.capture({}));\"}"}
            }]})
        } else {
            json!({"role":"assistant", "content":null, "tool_calls":[{
                "id":call_id, "type":"function", "function":{"name":"wait","arguments":"{}"}
            }]})
        });
        expected_openai.extend([
            json!({"role":"tool", "tool_call_id":call_id, "content":marker}),
            json!({"role":"user", "content":[
                {"type":"text","text":marker},
                {"type":"text","text":"image before yield"},
                {"type":"image_url", "image_url":{"url":"data:image/png;base64,iVBORw0KGgo=", "detail":"original"}},
                {"type":"text","text":"image after yield"}
            ]})
        ]);
    }
    assert_eq!(anthropic_results, expected);
    assert_eq!(openai["messages"], json!(expected_openai));
}

#[test]
fn audio_uses_the_supported_wire_route_and_retains_its_call_source() {
    let data = "UklGRjQAAABXQVZFZm10IBAAAAABAAEAQB8AAIA+AAACABAAZGF0YRAAAAAAAAAAAAAAAAAAAAAAAAAA";
    let bytes = payload(
        &json!([{"type":"input_audio", "audio_url":format!("data:audio/wav;base64,{data}")}]),
    );
    let options = AnthropicMessagesOptions {
        max_output_tokens: 4096,
        pricing: ProviderTokenPricing::default(),
    };
    let openai: Value = serde_json::from_slice(
        &prepare_openai_chat_request(&bytes, "fixture", options)
            .unwrap()
            .body,
    )
    .unwrap();
    let media = openai["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|message| message["content"].as_array())
        .filter(|content| content.iter().any(|block| block["type"] == "input_audio"))
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(
        media,
        ["exec-1", "wait-1"].map(|id| vec![
            json!({"type":"text","text":format!("Tool call source_id: {id}")}),
            json!({"type":"input_audio","input_audio":{"format":"wav","data":data}})
        ])
    );
    let error = prepare_anthropic_request(&bytes, "fixture", options)
        .err()
        .unwrap();
    assert_eq!(
        error.diagnostic(),
        Some(AnthropicCodecDiagnostic {
            stage: "unsupported_media",
            event_type: "input_audio",
            field_path: "$.request.input[]"
        })
    );
}
