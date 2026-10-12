// SPDX-License-Identifier: Apache-2.0

use super::*;
use std::fmt::Write;

fn options() -> AnthropicMessagesOptions {
    AnthropicMessagesOptions {
        max_output_tokens: 4096,
        pricing: ProviderTokenPricing::default(),
    }
}

fn tool_bindings() -> AnthropicToolBindings {
    let mut bindings = AnthropicToolBindings::default();
    for (name, kind) in [
        ("read_file", ProviderToolKind::Function),
        ("apply_patch", ProviderToolKind::Custom),
    ] {
        bindings
            .insert(
                name.to_owned(),
                ProviderToolIdentity::try_new(kind, name.to_owned(), None).unwrap(),
            )
            .unwrap();
    }
    bindings
}

fn ping() -> Value {
    json!({"type":"ping"})
}

fn complete_events() -> Vec<Value> {
    vec![
        json!({"type":"message_start", "message":{
            "id":"heartbeat-response", "type":"message", "role":"assistant", "model":"fixture-model",
            "usage":{"input_tokens":11,"cache_read_input_tokens":2,"cache_creation_input_tokens":3,"output_tokens":0}
        }}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"fixture reasoning"}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}),
        json!({"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"fixture answer"}}),
        json!({"type":"content_block_stop","index":1}),
        json!({"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"function-call","name":"read_file","input":{}}}),
        json!({"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{\"path\":\"fixture.rs\"}"}}),
        json!({"type":"content_block_stop","index":2}),
        json!({"type":"content_block_start","index":3,"content_block":{"type":"tool_use","id":"custom-call","name":"apply_patch","input":{}}}),
        json!({"type":"content_block_delta","index":3,"delta":{"type":"input_json_delta","partial_json":"{\"input\":\"fixture patch\"}"}}),
        json!({"type":"content_block_stop","index":3}),
        json!({"type":"message_delta","delta":{"stop_reason":"tool_use","stop_sequence":null},"usage":{"output_tokens":7}}),
        json!({"type":"message_stop"}),
    ]
}

fn wire(events: &[Value]) -> Vec<u8> {
    let mut result = String::new();
    for event in events {
        write!(
            result,
            "event: {}\ndata: {event}\n\n",
            event["type"].as_str().unwrap()
        )
        .unwrap();
    }
    result.into_bytes()
}

fn parse(events: &[Value]) -> Result<ParsedAnthropicStream, AnthropicCodecError> {
    parse_anthropic_sse(&wire(events), 64 * 1024, 256, &tool_bindings(), options())
}

fn observed(events: &[Value]) -> Option<(String, ProviderTokenUsage)> {
    observed_anthropic_receipt(&wire(events), 64 * 1024, 256, options())
}

fn assert_same_message_and_accounting(events: &[Value]) {
    let baseline = complete_events();
    let expected =
        parse(&baseline).expect("complete baseline with actual function/custom bindings");
    let actual = parse(events).expect("heartbeat must not change message lifecycle");
    assert_eq!(actual.events, expected.events);
    assert_eq!(actual.terminal, expected.terminal);
    assert_eq!(observed(events), observed(&baseline));
    let ProviderGatewayTerminal::Completed { usage, .. } = actual.terminal else {
        panic!("heartbeat must preserve Completed");
    };
    assert_eq!(usage, observed(events).unwrap().1);
    assert_eq!(usage.input_tokens, 16);
    assert_eq!(usage.cached_input_tokens, Some(2));
    assert_eq!(usage.cache_write_input_tokens, 3);
    assert_eq!(usage.output_tokens, 7);
}

#[test]
fn leading_ping_preserves_complete_message_and_observed_usage() {
    let mut events = vec![ping()];
    events.extend(complete_events());
    assert_same_message_and_accounting(&events);
}

#[test]
fn multiple_leading_pings_do_not_create_business_events() {
    let mut events = vec![ping(); 8];
    events.extend(complete_events());
    assert_same_message_and_accounting(&events);
}

#[test]
fn interleaved_pings_preserve_text_reasoning_function_custom_tools_and_usage() {
    let baseline = complete_events();
    let mut events = Vec::new();
    for (index, event) in baseline.iter().enumerate() {
        events.push(event.clone());
        if index + 1 < baseline.len() {
            events.extend([ping(), ping()]);
        }
    }
    assert_same_message_and_accounting(&events);
}

#[test]
fn trailing_pings_preserve_completed_terminal_without_business_events() {
    let mut events = complete_events();
    events.extend([ping(), ping(), ping()]);
    assert_same_message_and_accounting(&events);
}

#[test]
fn heartbeat_at_every_boundary_preserves_both_parse_routes() {
    let mut events = vec![ping()];
    for event in complete_events() {
        events.extend([event, ping()]);
    }
    assert_same_message_and_accounting(&events);
}

#[test]
fn ping_only_stream_is_incomplete_and_has_no_observed_receipt() {
    let events = vec![ping(); 3];
    let error = parse(&events)
        .err()
        .expect("heartbeats cannot complete a message");
    assert_eq!(error.kind(), AnthropicCodecErrorKind::IncompleteStream);
    assert_eq!(observed(&events), None);
}

#[test]
fn heartbeat_does_not_replace_required_message_start() {
    let mut events = complete_events();
    events.remove(0);
    events.insert(0, ping());
    assert!(parse(&events).is_err());
    assert_eq!(observed(&events), None);
}

#[test]
fn heartbeat_does_not_replace_required_message_stop_but_keeps_real_usage() {
    let baseline = complete_events();
    let mut events = baseline.clone();
    events.pop();
    events.push(ping());
    let error = parse(&events).err().expect("message_stop remains required");
    assert_eq!(error.kind(), AnthropicCodecErrorKind::IncompleteStream);
    assert_eq!(observed(&events), observed(&baseline));
}

#[test]
fn real_business_event_after_terminal_still_fails_closed() {
    for event in [
        complete_events()[0].clone(),
        json!({"type":"content_block_start","index":4,"content_block":{"type":"text","text":""}}),
        json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":8}}),
        json!({"type":"error","error":{"type":"api_error","message":"fixture failure"}}),
    ] {
        let mut events = complete_events();
        events.push(event);
        let error = parse(&events)
            .err()
            .expect("business events after terminal remain invalid");
        assert_eq!(error.kind(), AnthropicCodecErrorKind::Protocol);
    }
}

#[test]
fn heartbeats_preserve_typed_error_terminal_before_and_after_terminal() {
    let failure =
        json!({"type":"error","error":{"type":"rate_limit_error","message":"fixture failure"}});
    let expected = parse(std::slice::from_ref(&failure)).unwrap();
    for events in [
        vec![ping(), failure.clone()],
        vec![failure.clone(), ping()],
        vec![ping(), failure, ping()],
    ] {
        let actual = parse(&events).expect("heartbeats preserve genuine upstream failure");
        assert_eq!(actual.events, expected.events);
        assert_eq!(actual.terminal, expected.terminal);
        assert!(matches!(
            actual.terminal,
            ProviderGatewayTerminal::Failed { .. }
        ));
        assert_eq!(observed(&events), None);
    }
}

#[test]
fn malformed_ping_json_or_mismatched_type_still_fails_closed() {
    for prefix in [
        b"event: ping\ndata: {\n\n".as_slice(),
        b"event: ping\ndata: {\"type\":\"message_stop\"}\n\n".as_slice(),
        b"event: ping\ndata: {\"type\":null}\n\n".as_slice(),
    ] {
        let mut body = prefix.to_vec();
        body.extend(wire(&complete_events()));
        assert!(parse_anthropic_sse(&body, 64 * 1024, 256, &tool_bindings(), options()).is_err());
    }
}

#[test]
fn heartbeat_count_and_size_limits_apply_before_both_parse_routes() {
    let mut events = vec![ping(); 6];
    events.extend(complete_events());
    let body = wire(&events);
    let error = parse_anthropic_sse(&body, 64 * 1024, 5, &tool_bindings(), options())
        .err()
        .expect("heartbeats count toward the transport event limit");
    assert_eq!(error.kind(), AnthropicCodecErrorKind::SizeLimit);
    assert_eq!(
        observed_anthropic_receipt(&body, 64 * 1024, 5, options()),
        None
    );

    let mut events = vec![json!({"type":"ping","metadata":"x".repeat(1024)})];
    events.extend(complete_events());
    let body = wire(&events);
    let error = parse_anthropic_sse(&body, 512, 256, &tool_bindings(), options())
        .err()
        .expect("heartbeat payload bytes remain bounded");
    assert_eq!(error.kind(), AnthropicCodecErrorKind::SizeLimit);
    assert_eq!(observed_anthropic_receipt(&body, 512, 256, options()), None);
}
