// SPDX-License-Identifier: Apache-2.0

use super::*;

fn usage() -> Value {
    json!({
        "prompt_tokens": 16, "completion_tokens": 10, "total_tokens": 26,
        "prompt_tokens_details": {
            "cached_tokens": 6, "cache_write_tokens": 3, "audio_tokens": 0
        },
        "completion_tokens_details": {
            "reasoning_tokens": 4, "audio_tokens": 0,
            "accepted_prediction_tokens": 0, "rejected_prediction_tokens": 0
        }
    })
}

fn wire(usage: &Value) -> Vec<u8> {
    let stop = json!({
        "id": "usage-response", "object": "chat.completion.chunk",
        "model": "glm-5.3-flash",
        "choices": [{"index": 0, "delta": {"content": "ok"}, "finish_reason": "stop"}]
    });
    let receipt = json!({
        "id": "usage-response", "object": "chat.completion.chunk",
        "model": "glm-5.3-flash", "choices": [], "usage": usage
    });
    format!("data: {stop}\n\ndata: {receipt}\n\ndata: [DONE]\n\n").into_bytes()
}

fn parse(bytes: &[u8]) -> Result<ParsedOpenAiStream, AnthropicCodecError> {
    parse_openai_chat_sse(
        bytes,
        4096,
        16,
        &AnthropicToolBindings::default(),
        AnthropicMessagesOptions {
            max_output_tokens: 1024,
            pricing: ProviderTokenPricing::default(),
        },
    )
}

#[test]
fn usage_extensions_complete_usage_only_tail_and_accounting() {
    // The real failed preflight had these extra fields with zero values.
    let mut reported = usage();
    reported["prompt_tokens_details"]["cache_write_tokens"] = json!(0);
    let bytes = wire(&reported);
    let parsed = parse(&bytes).expect("valid complete upstream SSE");
    let ProviderGatewayTerminal::Completed {
        usage,
        actual_cost_micros,
    } = parsed.terminal
    else {
        panic!("expected completed response");
    };
    assert_eq!(usage.input_tokens, 16);
    assert_eq!(usage.cached_input_tokens, Some(6));
    assert_eq!(usage.output_tokens, 10);
    assert_eq!(usage.reasoning_output_tokens, 4);
    assert_eq!(actual_cost_micros, None);
    assert_eq!(
        observed_openai_usage(&bytes, 4096, 16),
        Some(("usage-response".into(), usage))
    );
}

#[test]
fn usage_extensions_preserve_cache_writes_and_do_not_double_count() {
    let reported = usage();
    let parsed = openai_usage(&reported).expect("cache write usage");
    assert_eq!(parsed.cache_write_input_tokens, 3);
    let pricing = ProviderTokenPricing {
        input_micros_per_million_tokens: 2_000_000,
        cached_input_micros_per_million_tokens: 1_000_000,
        cache_write_micros_per_million_tokens: 3_000_000,
        output_micros_per_million_tokens: 4_000_000,
        reasoning_output_micros_per_million_tokens: 0,
    };
    // Seven standard input tokens, six cache reads, three writes, ten output.
    assert_eq!(pricing.cost_micros(parsed).unwrap(), Some(69));
}

#[test]
fn usage_extensions_ignore_unconsumed_additive_metadata() {
    let mut extended = usage();
    extended["vendor_metadata"] = json!({"version": 2});
    extended["prompt_tokens_details"]["image_tokens"] = json!(0);
    extended["prompt_tokens_details"]["future_detail"] = json!([1, 2]);
    extended["completion_tokens_details"]["future_detail"] = json!({"opaque": true});
    assert_eq!(
        openai_usage(&extended).unwrap(),
        openai_usage(&usage()).unwrap()
    );
    assert!(matches!(
        parse(&wire(&extended)).unwrap().terminal,
        ProviderGatewayTerminal::Completed { .. }
    ));
}

#[test]
fn usage_extensions_do_not_turn_malformed_stream_into_success() {
    let mut bytes = wire(&usage());
    bytes.truncate(bytes.len() - "data: [DONE]\n\n".len());
    bytes.extend_from_slice(b"data: {broken}\n\ndata: [DONE]\n\n");
    assert!(parse(&bytes).is_err());
    let observed =
        observed_openai_usage(&bytes, 4096, 16).expect("observed usage remains chargeable");
    assert_eq!(observed.1.cache_write_input_tokens, 3);
    let mut drifted = String::from_utf8(wire(&usage())).unwrap();
    drifted = drifted.replacen("\"id\":\"usage-response\"", "\"id\":\"other-response\"", 1);
    assert!(observed_openai_usage(drifted.as_bytes(), 4096, 16).is_none());
}

#[test]
fn usage_extensions_accept_semantically_complete_eof_without_done() {
    let mut bytes = wire(&usage());
    bytes.truncate(bytes.len() - "data: [DONE]\n\n".len());
    assert!(matches!(
        parse(&bytes).unwrap().terminal,
        ProviderGatewayTerminal::Completed { .. }
    ));
}

#[test]
fn usage_extensions_nullable_optional_counters_remain_unknown() {
    let mut reported = usage();
    reported["prompt_tokens_details"]["cached_tokens"] = Value::Null;
    reported["prompt_tokens_details"]["cache_write_tokens"] = Value::Null;
    reported["completion_tokens_details"]["reasoning_tokens"] = Value::Null;
    let parsed = openai_usage(&reported).unwrap();
    assert_eq!(parsed.cached_input_tokens, None);
    assert_eq!(parsed.cache_write_input_tokens, 0);
    assert_eq!(parsed.reasoning_output_tokens, 0);
}

#[test]
fn usage_extensions_reject_invalid_consumed_counters_and_totals() {
    for (path, invalid) in [
        ("/prompt_tokens", json!(-1)),
        ("/completion_tokens", json!(1.5)),
        ("/total_tokens", json!(25)),
        ("/prompt_tokens", json!(9_007_199_254_740_992_u64)),
        ("/prompt_tokens_details", json!([])),
        ("/completion_tokens_details", json!("details")),
        ("/prompt_tokens_details/cached_tokens", json!("6")),
        ("/prompt_tokens_details/cache_write_tokens", json!(-1)),
        ("/prompt_tokens_details/cache_write_tokens", json!(11)),
        ("/completion_tokens_details/reasoning_tokens", json!(11)),
    ] {
        let mut reported = usage();
        *reported.pointer_mut(path).expect("existing usage field") = invalid;
        assert!(openai_usage(&reported).is_err(), "{path}");
        assert!(parse(&wire(&reported)).is_err(), "stream {path}");
        assert!(
            observed_openai_usage(&wire(&reported), 4096, 16).is_none(),
            "accounting {path}"
        );
    }
}

#[test]
fn usage_extensions_preserve_deepseek_cache_consistency() {
    let mut reported = usage();
    reported["prompt_cache_hit_tokens"] = json!(6);
    reported["prompt_cache_miss_tokens"] = json!(10);
    assert_eq!(openai_usage(&reported).unwrap().cache_write_input_tokens, 3);
    reported["prompt_cache_hit_tokens"] = json!(5);
    assert!(openai_usage(&reported).is_err());
}

#[test]
fn usage_extensions_reject_unsafe_combined_token_total() {
    let reported = json!({"prompt_tokens":9_007_199_254_740_991_u64,"completion_tokens":1});
    assert!(openai_usage(&reported).is_err());
}

#[test]
fn usage_extensions_report_consumed_field_path_in_stream_diagnostic() {
    let mut reported = usage();
    reported["prompt_tokens_details"]["cache_write_tokens"] = json!("3");
    let error = parse(&wire(&reported))
        .err()
        .expect("invalid write counter");
    let diagnostic = error.diagnostic().expect("safe field diagnostic");
    assert_eq!(diagnostic.stage, "response_fields");
    assert_eq!(diagnostic.event_type, "chat.completion.chunk");
    assert_eq!(
        diagnostic.field_path,
        "$.usage.prompt_tokens_details.cache_write_tokens"
    );
}
