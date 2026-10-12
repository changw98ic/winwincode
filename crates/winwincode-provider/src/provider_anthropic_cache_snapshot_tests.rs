// SPDX-License-Identifier: Apache-2.0

use super::*;
use std::fmt::Write;

fn options() -> AnthropicMessagesOptions {
    AnthropicMessagesOptions {
        max_output_tokens: 4096,
        pricing: ProviderTokenPricing {
            input_micros_per_million_tokens: 1_000_000,
            cached_input_micros_per_million_tokens: 100_000,
            cache_write_micros_per_million_tokens: 2_000_000,
            output_micros_per_million_tokens: 3_000_000,
            reasoning_output_micros_per_million_tokens: 0,
        },
    }
}

fn wire(initial: Value, final_usage: Value) -> Vec<u8> {
    let mut events = [
        json!({"type":"message_start","message":{
            "id":"cache-snapshot-response","type":"message","role":"assistant","model":"fixture-model","usage":{}
        }}),
        json!({"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{}}),
        json!({"type":"message_stop"}),
    ];
    events[0]["message"]["usage"] = initial;
    events[1]["usage"] = final_usage;
    let mut body = String::new();
    for event in events {
        write!(
            body,
            "event: {}\ndata: {event}\n\n",
            event["type"].as_str().unwrap()
        )
        .unwrap();
    }
    body.into_bytes()
}

fn assert_completed_snapshot(
    initial: Value,
    final_usage: Value,
    expected: ProviderTokenUsage,
    cost: u64,
) {
    let body = wire(initial, final_usage);
    let parsed = parse_anthropic_sse(
        &body,
        64 * 1024,
        64,
        &AnthropicToolBindings::default(),
        options(),
    )
    .expect("cumulative input total may reclassify components");
    assert_eq!(
        parsed.terminal,
        ProviderGatewayTerminal::Completed {
            usage: expected,
            actual_cost_micros: Some(cost),
        }
    );
    assert_eq!(
        observed_anthropic_receipt(&body, 64 * 1024, 64, options()),
        Some(("cache-snapshot-response".to_owned(), expected))
    );
    assert!(
        parsed
            .events
            .contains(&ProviderStreamEvent::Usage(expected))
    );
}

fn usage(total: u64, read: u64, write: u64, output: u64) -> ProviderTokenUsage {
    ProviderTokenUsage {
        input_tokens: total,
        cached_input_tokens: Some(read),
        cache_write_input_tokens: write,
        output_tokens: output,
        reasoning_output_tokens: 0,
    }
}

fn assert_invalid(initial: Value, final_usage: Value) {
    let body = wire(initial, final_usage);
    let error = parse_anthropic_sse(
        &body,
        64 * 1024,
        64,
        &AnthropicToolBindings::default(),
        options(),
    )
    .err()
    .expect("invalid counters must fail closed");
    assert_eq!(error.kind(), AnthropicCodecErrorKind::Protocol);
    assert_eq!(
        observed_anthropic_receipt(&body, 64 * 1024, 64, options()),
        None
    );
}

#[test]
fn final_cache_snapshot_reclassifies_standard_input_with_growing_total() {
    assert_completed_snapshot(
        json!({"input_tokens":12986,"cache_read_input_tokens":0,"cache_creation_input_tokens":0,"output_tokens":0}),
        json!({"input_tokens":72,"cache_read_input_tokens":13312,"cache_creation_input_tokens":0,"output_tokens":29}),
        usage(13384, 13312, 0, 29),
        1490,
    );
}

#[test]
fn same_total_can_move_between_standard_read_and_write_each_direction() {
    for (initial, final_usage, expected, cost) in [
        (
            json!({"input_tokens":10,"output_tokens":0}),
            json!({"input_tokens":0,"cache_read_input_tokens":10,"output_tokens":7}),
            usage(10, 10, 0, 7),
            22,
        ),
        (
            json!({"input_tokens":0,"cache_read_input_tokens":10,"output_tokens":0}),
            json!({"input_tokens":10,"cache_read_input_tokens":0,"output_tokens":7}),
            usage(10, 0, 0, 7),
            31,
        ),
        (
            json!({"input_tokens":10,"output_tokens":0}),
            json!({"input_tokens":0,"cache_creation_input_tokens":10,"output_tokens":7}),
            usage(10, 0, 10, 7),
            41,
        ),
        (
            json!({"input_tokens":0,"cache_creation_input_tokens":10,"output_tokens":0}),
            json!({"input_tokens":10,"cache_creation_input_tokens":0,"output_tokens":7}),
            usage(10, 0, 0, 7),
            31,
        ),
        (
            json!({"input_tokens":0,"cache_read_input_tokens":10,"output_tokens":0}),
            json!({"cache_read_input_tokens":0,"cache_creation_input_tokens":10,"output_tokens":7}),
            usage(10, 0, 10, 7),
            41,
        ),
        (
            json!({"input_tokens":0,"cache_creation_input_tokens":10,"output_tokens":0}),
            json!({"cache_read_input_tokens":10,"cache_creation_input_tokens":0,"output_tokens":7}),
            usage(10, 10, 0, 7),
            22,
        ),
    ] {
        assert_completed_snapshot(initial, final_usage, expected, cost);
    }
}

#[test]
fn partial_delta_inherits_absent_standard_and_cache_write_components() {
    assert_completed_snapshot(
        json!({"input_tokens":10,"cache_read_input_tokens":3,"cache_creation_input_tokens":2,"output_tokens":1}),
        json!({"cache_read_input_tokens":4,"output_tokens":7}),
        usage(16, 4, 2, 7),
        35,
    );
}

#[test]
fn partial_delta_inherits_both_absent_cache_components() {
    assert_completed_snapshot(
        json!({"input_tokens":10,"cache_read_input_tokens":3,"cache_creation_input_tokens":2,"output_tokens":1}),
        json!({"input_tokens":12,"output_tokens":7}),
        usage(17, 3, 2, 7),
        37,
    );
}

#[test]
fn provided_components_replace_prior_snapshot_without_double_counting() {
    assert_completed_snapshot(
        json!({"input_tokens":10,"cache_read_input_tokens":4,"cache_creation_input_tokens":3,"output_tokens":0}),
        json!({"input_tokens":6,"cache_read_input_tokens":8,"cache_creation_input_tokens":3,"output_tokens":7}),
        usage(17, 8, 3, 7),
        33,
    );
}

#[test]
fn genuinely_decreased_total_input_remains_invalid() {
    assert_invalid(
        json!({"input_tokens":10,"cache_read_input_tokens":3,"cache_creation_input_tokens":2,"output_tokens":0}),
        json!({"input_tokens":2,"cache_read_input_tokens":8,"cache_creation_input_tokens":1,"output_tokens":7}),
    );
}

#[test]
fn genuinely_decreased_output_remains_invalid_even_with_input_reclassification() {
    assert_invalid(
        json!({"input_tokens":10,"output_tokens":4}),
        json!({"input_tokens":0,"cache_read_input_tokens":10,"output_tokens":3}),
    );
}

#[test]
fn negative_fractional_nonnumeric_and_unsafe_counter_values_remain_invalid() {
    for field in [
        "input_tokens",
        "cache_read_input_tokens",
        "cache_creation_input_tokens",
        "output_tokens",
    ] {
        for invalid in [
            json!(-1),
            json!(1.5),
            json!("3"),
            json!(true),
            json!({"tokens":3}),
            json!([3]),
            json!(9_007_199_254_740_992_u64),
        ] {
            let mut final_usage = json!({"input_tokens":2,"cache_read_input_tokens":2,"cache_creation_input_tokens":2,"output_tokens":3});
            final_usage[field] = invalid;
            assert_invalid(json!({"input_tokens":1,"output_tokens":0}), final_usage);
        }
    }
}

#[test]
fn checked_input_sum_and_safe_total_limit_remain_enforced() {
    for final_usage in [
        json!({"input_tokens":9_007_199_254_740_991_u64,"cache_read_input_tokens":1,"output_tokens":3}),
        json!({"input_tokens":u64::MAX,"cache_read_input_tokens":1,"output_tokens":3}),
    ] {
        assert_invalid(json!({"input_tokens":1,"output_tokens":0}), final_usage);
    }
}

#[test]
fn reclassified_observed_receipt_survives_invalid_business_suffix_without_success() {
    let mut body = wire(
        json!({"input_tokens":10,"output_tokens":0}),
        json!({"input_tokens":0,"cache_read_input_tokens":10,"output_tokens":7}),
    );
    body.extend_from_slice(b"event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"fixture suffix\"}}\n\n");
    assert!(
        parse_anthropic_sse(
            &body,
            64 * 1024,
            64,
            &AnthropicToolBindings::default(),
            options()
        )
        .is_err()
    );
    assert_eq!(
        observed_anthropic_receipt(&body, 64 * 1024, 64, options()),
        Some(("cache-snapshot-response".to_owned(), usage(10, 10, 0, 7)))
    );
}
