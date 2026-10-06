// SPDX-License-Identifier: Apache-2.0

//! `ChatGPT` Responses transport to the embedded Core's `ModelPort` event format.
//! Preserve response item IDs, message phase and encrypted reasoning for later turns.

use crate::{
    CanonicalModelStreamFrame, CredentialOutputBoundary, HttpsSseProviderCompletion,
    HttpsSseProviderError, HttpsSseProviderErrorKind, ModelAttemptFailureFact,
    ModelExecutionCertainty, ProviderGatewayOpenReceipt, ProviderGatewayTerminal,
    ProviderStreamFailureKind, ProviderTokenUsage,
};
use serde_json::{Value, json};
use std::collections::BTreeSet;

fn protocol() -> HttpsSseProviderError {
    HttpsSseProviderError::new(HttpsSseProviderErrorKind::SseEvent)
}

pub(crate) fn prepare_request(
    payload: &[u8],
    model: &str,
) -> Result<Vec<u8>, HttpsSseProviderError> {
    prepare(payload, model, true)
}

pub(crate) fn prepare_plan_request(
    payload: &[u8],
    model: &str,
) -> Result<Vec<u8>, HttpsSseProviderError> {
    prepare(payload, model, false)
}

fn prepare(
    payload: &[u8],
    model: &str,
    codex_backend: bool,
) -> Result<Vec<u8>, HttpsSseProviderError> {
    let envelope: Value = serde_json::from_slice(payload).map_err(|_| protocol())?;
    let mut request = envelope
        .get("request")
        .and_then(Value::as_object)
        .cloned()
        .ok_or_else(protocol)?;
    if request.get("model").and_then(Value::as_str) != Some(model)
        || !request.get("input").is_some_and(Value::is_array)
    {
        return Err(protocol());
    }
    request.insert("stream".into(), json!(true));
    request.insert("store".into(), json!(false));
    request.entry("instructions").or_insert_with(|| json!(""));
    // The subscription endpoint rejects API-only sampling/output-limit parameters.
    if codex_backend {
        for key in ["max_output_tokens", "temperature", "top_p"] {
            request.remove(key);
        }
    }
    serde_json::to_vec(&request).map_err(|_| protocol())
}

pub(crate) fn parse_response(
    bytes: &[u8],
    receipt: &ProviderGatewayOpenReceipt,
    max_event_bytes: usize,
    max_events: usize,
) -> Result<HttpsSseProviderCompletion, HttpsSseProviderError> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| protocol())?
        .replace("\r\n", "\n");
    if text.contains(['\0', '\r']) {
        return Err(protocol());
    }
    let mut stream = Responses::new(receipt);
    let mut data = String::new();
    let mut count = 0;
    for line in text.split('\n').chain(std::iter::once("")) {
        if line.len() > max_event_bytes {
            return Err(HttpsSseProviderError::new(
                HttpsSseProviderErrorKind::SizeLimit,
            ));
        }
        if line.is_empty() && !data.is_empty() {
            count += 1;
            if count > max_events {
                return Err(HttpsSseProviderError::new(
                    HttpsSseProviderErrorKind::SizeLimit,
                ));
            }
            stream.push(&serde_json::from_str(&data).map_err(|_| protocol())?)?;
            data.clear();
        } else if let Some(value) = line.strip_prefix("data:") {
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(value.strip_prefix(' ').unwrap_or(value));
            if data.len() > max_event_bytes {
                return Err(HttpsSseProviderError::new(
                    HttpsSseProviderErrorKind::SizeLimit,
                ));
            }
        } else if !line.is_empty() && !line.starts_with(':') && !line.starts_with("event:") {
            return Err(protocol());
        }
    }
    stream.finish()
}

struct Responses<'a> {
    receipt: &'a ProviderGatewayOpenReceipt,
    frames: Vec<CanonicalModelStreamFrame>,
    response_id: Option<String>,
    items: BTreeSet<String>,
    done: BTreeSet<String>,
    terminal: Option<ProviderGatewayTerminal>,
}

impl<'a> Responses<'a> {
    fn new(receipt: &'a ProviderGatewayOpenReceipt) -> Self {
        Self {
            receipt,
            frames: Vec::new(),
            response_id: None,
            items: BTreeSet::new(),
            done: BTreeSet::new(),
            terminal: None,
        }
    }

    fn emit(&mut self, value: &Value, terminal: bool) -> Result<(), HttpsSseProviderError> {
        let payload = serde_json::to_string(&value).map_err(|_| protocol())?;
        self.receipt
            .stream_leak_gate
            .inspect_bytes(CredentialOutputBoundary::Event, payload.as_bytes())
            .map_err(|_| HttpsSseProviderError::new(HttpsSseProviderErrorKind::CredentialLeak))?;
        self.frames
            .push(CanonicalModelStreamFrame::from_codex_event(
                self.frames.len() as u64,
                payload,
                terminal,
            ));
        Ok(())
    }

    #[allow(clippy::too_many_lines)]
    fn push(&mut self, event: &Value) -> Result<(), HttpsSseProviderError> {
        if self.terminal.is_some() {
            return Err(protocol());
        }
        let kind = event
            .get("type")
            .and_then(Value::as_str)
            .ok_or_else(protocol)?;
        if kind == "response.created" {
            if self.response_id.is_some() {
                return Err(protocol());
            }
            self.response_id = Some(string(event, "/response/id")?.to_owned());
            self.emit(&json!({"type":"created"}), false)?;
            return self.emit(
                &json!({"type":"server_model","model":self.receipt.route.model_id}),
                false,
            );
        }
        if matches!(kind, "response.failed" | "response.incomplete" | "error") {
            return self.fail();
        }
        if self.response_id.is_none() {
            return Err(protocol());
        }
        let mapped = match kind {
            "response.output_item.added" | "response.output_item.done" => {
                let item = event
                    .get("item")
                    .filter(|item| item.is_object())
                    .ok_or_else(protocol)?;
                let id = string(item, "/id")?.to_owned();
                if !matches!(
                    string(item, "/type")?,
                    "message" | "reasoning" | "function_call" | "custom_tool_call"
                ) {
                    return Err(protocol());
                }
                if kind == "response.output_item.added" {
                    if self.items.len() >= 4096 || !self.items.insert(id) {
                        return Err(protocol());
                    }
                } else if !self.items.contains(&id) || !self.done.insert(id) {
                    return Err(protocol());
                }
                Some(
                    json!({"type":if kind.ends_with("added"){"output_item_added"}else{"output_item_done"},"item":item}),
                )
            }
            "response.output_text.delta" => {
                Some(json!({"type":"output_text_delta","delta":string(event,"/delta")?}))
            }
            "response.custom_tool_call_input.delta" => Some(
                json!({"type":"tool_call_input_delta","itemId":string(event,"/item_id")?,"callId":event.get("call_id"),"delta":string(event,"/delta")?}),
            ),
            "response.reasoning_summary_part.added" => Some(
                json!({"type":"reasoning_summary_part_added","summaryIndex":number(event,"/summary_index")?}),
            ),
            "response.reasoning_summary_text.delta" => Some(
                json!({"type":"reasoning_summary_delta","summaryIndex":number(event,"/summary_index")?,"delta":string(event,"/delta")?}),
            ),
            "response.reasoning_summary_text.done" => Some(
                json!({"type":"reasoning_summary_done","summaryIndex":number(event,"/summary_index")?,"itemId":string(event,"/item_id")?,"text":string(event,"/text")?}),
            ),
            "response.reasoning_text.delta" => Some(
                json!({"type":"reasoning_content_delta","contentIndex":number(event,"/content_index")?,"delta":string(event,"/delta")?}),
            ),
            "response.completed" => {
                if Some(string(event, "/response/id")?) != self.response_id.as_deref()
                    || self.done.is_empty()
                    || self.done != self.items
                {
                    return Err(protocol());
                }
                let usage = ProviderTokenUsage {
                    input_tokens: number(event, "/response/usage/input_tokens")?,
                    output_tokens: number(event, "/response/usage/output_tokens")?,
                    cached_input_tokens: optional_cache_number(
                        event,
                        "/response/usage/input_tokens_details/cached_tokens",
                    )?,
                    cache_write_input_tokens: optional_number(
                        event,
                        "/response/usage/input_tokens_details/cache_write_tokens",
                    )?,
                    reasoning_output_tokens: optional_number(
                        event,
                        "/response/usage/output_tokens_details/reasoning_tokens",
                    )?,
                };
                if usage
                    .cached_input_tokens
                    .is_some_and(|cached| cached > usage.input_tokens)
                    || usage.reasoning_output_tokens > usage.output_tokens
                {
                    return Err(protocol());
                }
                self.emit(&json!({"type":"completed","responseId":self.response_id,"tokenUsage":{
                    "input_tokens":usage.input_tokens,"cached_input_tokens":usage.cached_input_tokens.unwrap_or(0),"cache_write_input_tokens":usage.cache_write_input_tokens,
                    "output_tokens":usage.output_tokens,"reasoning_output_tokens":usage.reasoning_output_tokens,"total_tokens":usage.input_tokens+usage.output_tokens},"endTurn":event.pointer("/response/end_turn")}),true)?;
                self.terminal = Some(ProviderGatewayTerminal::Completed {
                    usage,
                    actual_cost_micros: None,
                });
                None
            }
            // These events carry bookkeeping already present in completed response items.
            "response.in_progress"
            | "response.content_part.added"
            | "response.content_part.done"
            | "response.output_text.done"
            | "response.function_call_arguments.delta"
            | "response.function_call_arguments.done"
            | "response.custom_tool_call_input.done"
            | "response.reasoning_summary_part.done"
            | "response.reasoning_text.done" => None,
            _ => return Err(protocol()),
        };
        if let Some(value) = mapped {
            self.emit(&value, false)?;
        }
        Ok(())
    }

    fn fail(&mut self) -> Result<(), HttpsSseProviderError> {
        self.emit(&json!({"type":"error","error":{"code":"CODEX_RESPONSE_FAILED","message":"Codex response failed or ended before completion"}}),true)?;
        self.terminal = Some(ProviderGatewayTerminal::Failed {
            failure: ModelAttemptFailureFact::from_stream(
                ProviderStreamFailureKind::Unknown,
                ModelExecutionCertainty::AcceptanceUnknown,
            ),
            charge: None,
        });
        Ok(())
    }

    fn finish(self) -> Result<HttpsSseProviderCompletion, HttpsSseProviderError> {
        if self.terminal.is_none() {
            return Err(HttpsSseProviderError::new(
                HttpsSseProviderErrorKind::IncompleteStream,
            ));
        }
        Ok(HttpsSseProviderCompletion {
            frames: self.frames,
            terminal: self.terminal.ok_or_else(protocol)?,
        })
    }
}

fn string<'a>(value: &'a Value, path: &str) -> Result<&'a str, HttpsSseProviderError> {
    value
        .pointer(path)
        .and_then(Value::as_str)
        .ok_or_else(protocol)
}
fn number(value: &Value, path: &str) -> Result<u64, HttpsSseProviderError> {
    value
        .pointer(path)
        .and_then(Value::as_u64)
        .filter(|value| *value <= 9_007_199_254_740_991)
        .ok_or_else(protocol)
}
fn optional_cache_number(value: &Value, path: &str) -> Result<Option<u64>, HttpsSseProviderError> {
    value.pointer(path).map(|_| number(value, path)).transpose()
}
fn optional_number(value: &Value, path: &str) -> Result<u64, HttpsSseProviderError> {
    if value.pointer(path).is_none() {
        Ok(0)
    } else {
        number(value, path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CredentialLeakGate, ResolvedSecret};
    use winwincode_api::generated::ModelRoute;
    use winwincode_domain::{CredentialReferenceId, ModelExchangeId, RequestId};

    fn receipt() -> ProviderGatewayOpenReceipt {
        ProviderGatewayOpenReceipt {
            model_exchange_id: ModelExchangeId("mdl_00000000000000000000000001".into()),
            request_id: RequestId("req_00000000000000000000000001".into()),
            route: ModelRoute {
                provider_id: "codex".into(),
                model_id: "test-model".into(),
                credential_reference_id: CredentialReferenceId(
                    "crd_00000000000000000000000001".into(),
                ),
            },
            adapter_request_id: "probe".into(),
            idempotent_replay: false,
            stream_leak_gate: CredentialLeakGate::new(),
        }
    }
    fn wire(events: &[Value]) -> Vec<u8> {
        use std::fmt::Write as _;
        let mut text = String::new();
        for event in events {
            writeln!(
                &mut text,
                "event: {}\ndata: {event}\n",
                event["type"].as_str().unwrap()
            )
            .unwrap();
        }
        text.into_bytes()
    }
    fn response_events() -> Vec<Value> {
        vec![
            json!({"type":"response.created","response":{"id":"resp-one"}}),
            json!({"type":"response.output_item.added","item":{"type":"reasoning","id":"rs-one","summary":[]}}),
            json!({"type":"response.output_item.done","item":{"type":"reasoning","id":"rs-one","summary":[],"encrypted_content":"encrypted-reasoning"}}),
            json!({"type":"response.output_item.added","item":{"type":"message","id":"msg-one","role":"assistant","content":[],"phase":"final_answer"}}),
            json!({"type":"response.output_item.done","item":{"type":"message","id":"msg-one","role":"assistant","content":[{"type":"output_text","text":"OK"}],"phase":"final_answer"}}),
            json!({"type":"response.completed","response":{"id":"resp-one","usage":{"input_tokens":10,"output_tokens":4,"input_tokens_details":{"cached_tokens":2},"output_tokens_details":{"reasoning_tokens":1}},"end_turn":true}}),
        ]
    }
    #[test]
    fn responses_preserve_phase_encrypted_reasoning_and_usage() {
        let completion = parse_response(&wire(&response_events()), &receipt(), 4096, 100).unwrap();
        assert_eq!(
            completion.terminal.outcome(),
            crate::ProviderGatewayTerminalOutcome::Succeeded
        );
        let payloads = completion
            .frames
            .iter()
            .map(CanonicalModelStreamFrame::payload_json)
            .collect::<String>();
        assert!(payloads.contains("encrypted-reasoning"));
        assert!(payloads.contains("final_answer"));
        let last: Value =
            serde_json::from_str(completion.frames.last().unwrap().payload_json()).unwrap();
        assert_eq!(last["tokenUsage"]["cached_input_tokens"], 2);
        assert_eq!(last["tokenUsage"]["total_tokens"], 14);
        assert!(completion.frames.last().unwrap().is_terminal());
    }
    #[test]
    fn missing_cache_usage_and_actual_cost_remain_unknown() {
        let mut events = response_events();
        events.last_mut().unwrap()["response"]["usage"] =
            json!({"input_tokens":10,"output_tokens":4});
        let completion = parse_response(&wire(&events), &receipt(), 4096, 100).unwrap();
        let ProviderGatewayTerminal::Completed {
            usage,
            actual_cost_micros,
        } = completion.terminal
        else {
            panic!("expected a completed response");
        };
        assert_eq!(usage.cached_input_tokens, None);
        assert_eq!(actual_cost_micros, None);
    }
    #[test]
    fn cutoff_identity_drift_and_echoed_credentials_fail_closed() {
        let mut events = response_events();
        events.pop();
        assert_eq!(
            parse_response(&wire(&events), &receipt(), 4096, 100)
                .unwrap_err()
                .kind(),
            HttpsSseProviderErrorKind::IncompleteStream
        );
        assert!(
            parse_response(&wire(&events), &receipt(), 4096, 100)
                .unwrap_err()
                .retryable()
        );
        events = response_events();
        events.last_mut().unwrap()["response"]["id"] = json!("other-response");
        assert!(parse_response(&wire(&events), &receipt(), 4096, 100).is_err());
        events = response_events();
        let mut receipt = receipt();
        receipt.stream_leak_gate.track_secret(
            &ResolvedSecret::from_bytes(b"private-login-test-token".to_vec()).unwrap(),
        );
        events[4]["item"]["content"][0]["text"] = json!("private-login-test-token");
        assert_eq!(
            parse_response(&wire(&events), &receipt, 4096, 100)
                .unwrap_err()
                .kind(),
            HttpsSseProviderErrorKind::CredentialLeak
        );
    }
    #[test]
    fn request_unwraps_envelope_and_requires_frozen_model() {
        let input = json!({"request":{"model":"test-model","input":[],"max_output_tokens":12,"store":true}});
        let payload = serde_json::to_vec(&input).unwrap();
        let request: Value =
            serde_json::from_slice(&prepare_request(&payload, "test-model").unwrap()).unwrap();
        assert_eq!(request["store"], false);
        assert_eq!(request["stream"], true);
        assert!(request.get("max_output_tokens").is_none());
        assert!(prepare_request(&payload, "other-model").is_err());
    }
}
