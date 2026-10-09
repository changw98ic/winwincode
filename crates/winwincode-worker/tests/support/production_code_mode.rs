// SPDX-License-Identifier: Apache-2.0

use std::collections::BTreeMap;

use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::Value;
use winwincode_control_plane::{ProviderStreamEvent, ProviderToolIdentity, ProviderToolKind};
use winwincode_execution_port::generated::ModelOpenMessage;

pub(super) fn events(
    events: impl IntoIterator<Item = ProviderStreamEvent>,
) -> Vec<ProviderStreamEvent> {
    let mut pending = BTreeMap::<u32, (ProviderToolIdentity, String)>::new();
    let mut output = Vec::new();
    for event in events {
        match event {
            ProviderStreamEvent::ToolCallStarted {
                index,
                provider_call_id,
                identity,
            } if !matches!(identity.name(), "exec" | "wait") => {
                pending.insert(index, (identity, String::new()));
                output.push(ProviderStreamEvent::ToolCallStarted {
                    index,
                    provider_call_id,
                    identity: ProviderToolIdentity::try_new(
                        ProviderToolKind::Custom,
                        "exec".to_owned(),
                        None,
                    )
                    .expect("Code Mode identity"),
                });
            }
            ProviderStreamEvent::ToolCallArgumentsDelta {
                index,
                provider_call_id,
                delta,
            } => {
                if let Some((_, input)) = pending.get_mut(&index) {
                    input.push_str(&delta);
                } else {
                    output.push(ProviderStreamEvent::ToolCallArgumentsDelta {
                        index,
                        provider_call_id,
                        delta,
                    });
                }
            }
            ProviderStreamEvent::ToolCallEnded {
                index,
                provider_call_id,
            } => {
                if let Some((identity, input)) = pending.remove(&index) {
                    let argument = match identity.kind() {
                        ProviderToolKind::Function => serde_json::from_str::<Value>(&input)
                            .expect("function arguments")
                            .to_string(),
                        ProviderToolKind::Custom => {
                            serde_json::to_string(&input).expect("freeform input")
                        }
                    };
                    let name = serde_json::to_string(identity.name()).unwrap();
                    let source = format!(
                        "// @exec: {{\"yield_time_ms\":10000}}\nconst tool = ALL_TOOLS.find(t => t.name.endsWith({name}));\n\
                         if (!tool) throw new Error('Required tool is unavailable');\n\
                         const result = await tools[tool.name]({argument});\n\
                         text(result?.output ?? result);\n\
                         if (Number.isInteger(result?.exit_code)) text('Exit code: ' + result.exit_code);"
                    );
                    output.push(ProviderStreamEvent::ToolCallArgumentsDelta {
                        index,
                        provider_call_id: provider_call_id.clone(),
                        delta: source,
                    });
                }
                output.push(ProviderStreamEvent::ToolCallEnded {
                    index,
                    provider_call_id,
                });
            }
            event => output.push(event),
        }
    }
    assert!(pending.is_empty(), "fixture tool calls must end");
    output
}

pub(super) fn source_id(open: &ModelOpenMessage, tool: &str) -> String {
    fn find(value: &Value, tool: &str, receipt_states: &mut Vec<[bool; 4]>) -> Option<String> {
        match value {
            Value::String(text) => {
                let (_, rest) = text.split_once("<core_tool_receipts>")?;
                let (body, _) = rest.split_once("</core_tool_receipts>")?;
                let fragment: Value = serde_json::from_str(body).ok()?;
                fragment["receipts"].as_array()?.iter().find_map(|receipt| {
                    if receipt["tool"].as_str()?.ends_with(tool) && receipt_states.len() < 16 {
                        receipt_states.push([
                            receipt["execution"] == "completed",
                            receipt["disposition"] == "accepted",
                            receipt["delivery"] == "offered",
                            receipt["source_id"].as_str().is_some(),
                        ]);
                    }
                    (receipt["tool"].as_str()?.ends_with(tool)
                        && receipt["execution"] == "completed"
                        && receipt["disposition"] == "accepted"
                        && receipt["delivery"] == "offered")
                        .then(|| receipt["source_id"].as_str().map(str::to_owned))
                        .flatten()
                })
            }
            Value::Array(values) => values
                .iter()
                .find_map(|value| find(value, tool, receipt_states)),
            Value::Object(values) => values
                .values()
                .find_map(|value| find(value, tool, receipt_states)),
            _ => None,
        }
    }
    let request: Value = serde_json::from_slice(
        &STANDARD
            .decode(&open.request.data_base64)
            .expect("model request bytes"),
    )
    .expect("model request JSON");
    let mut receipt_states = Vec::new();
    let mut tool_outputs = 0_usize;
    request["request"]["input"]
        .as_array()
        .expect("model input")
        .iter()
        .filter(|item| {
            matches!(
                item["type"].as_str(),
                Some("custom_tool_call_output" | "function_call_output")
            )
        })
        .find_map(|item| {
            tool_outputs += 1;
            find(&item["output"], tool, &mut receipt_states)
        })
        .unwrap_or_else(|| {
            panic!(
                "model receives the original completed Core tool source: tool_outputs={tool_outputs} \
                 receipt_states[completed,accepted,offered,source_present]={receipt_states:?}"
            )
        })
}
