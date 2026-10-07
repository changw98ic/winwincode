// SPDX-License-Identifier: Apache-2.0

use serde_json::Value;

pub(super) fn shell_source(command: &str) -> String {
    let input =
        serde_json::json!({"cmd": command, "workdir": ".", "login":false, "yield_time_ms":10000});
    format!(
        "// @exec: {{\"yield_time_ms\":10000}}\nconst tool = ALL_TOOLS.find(t => t.name.endsWith('exec_command'));\n\
         if (!tool) throw new Error('Shell tool is unavailable');\n\
         let result = await tools[tool.name]({input});\n\
         text(result.output);\n\
         while (Number.isInteger(result.session_id)) {{\n\
           result = await tools.write_stdin({{session_id:result.session_id,chars:'',yield_time_ms:10000}});\n\
           text(result.output);\n\
         }}\n\
         if (Number.isInteger(result.exit_code)) text('Exit code: ' + result.exit_code);"
    )
}

pub(super) fn shell_source_id(text: &str) -> Option<String> {
    let (_, tail) = text.split_once("<core_tool_receipts>")?;
    let (body, _) = tail.split_once("</core_tool_receipts>")?;
    let packet: Value = serde_json::from_str(body).ok()?;
    if packet["type"] != "core_tool_receipts" || packet["schema_version"] != 1 {
        return None;
    }
    packet["receipts"].as_array()?.iter().find_map(|receipt| {
        if receipt["tool"].as_str()?.ends_with("exec_command")
            && receipt["execution"] == "completed"
            && receipt["disposition"] == "accepted"
            && receipt["delivery"] == "offered"
        {
            receipt["source_id"].as_str().map(str::to_owned)
        } else {
            None
        }
    })
}

fn tool_outputs(request: &Value) -> impl DoubleEndedIterator<Item = &Value> {
    request
        .get("request")
        .unwrap_or(request)
        .get("input")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|item| {
            matches!(
                item["type"].as_str(),
                Some("custom_tool_call_output" | "function_call_output")
            )
        })
}

pub(super) fn active_cell(request: &Value) -> Option<String> {
    let text = super::tool_output_text(tool_outputs(request).next_back()?)?;
    let cell = text
        .lines()
        .next()?
        .strip_prefix("Script running with cell ID ")?;
    (!cell.is_empty()
        && cell.len() <= 80
        && cell
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_')))
    .then(|| cell.to_owned())
}

pub(super) fn verification_source_id(request: &Value) -> Option<String> {
    tool_outputs(request)
        .rev()
        .find_map(|output| shell_source_id(&super::tool_output_text(output)?))
}

pub(super) fn exit_code(request: &Value) -> Option<i64> {
    tool_outputs(request)
        .rev()
        .find_map(|output| super::parse_process_exit_code(&super::tool_output_text(output)?))
}
