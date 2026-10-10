// SPDX-License-Identifier: Apache-2.0

use serde_json::Value;

// Convert synthetic canonical model frames through the current public exec tool.
// This only scripts a model response; Core still discovers and executes the real tool.
pub(super) fn canonical_frames(frames: &[Value]) -> Vec<Value> {
    frames.iter().map(|frame| {
        let mut frame = frame.clone();
        let Some(item) = frame.get_mut("item") else {
            return frame;
        };
        let kind = item["type"].as_str();
        let name = item["name"].as_str().unwrap_or("");
        if !matches!(kind, Some("function_call" | "custom_tool_call"))
            || matches!(name, "exec" | "wait") {
            return frame;
        }
        let argument = if kind == Some("function_call") {
            serde_json::from_str::<Value>(item["arguments"].as_str().expect("tool arguments"))
                .expect("synthetic function input").to_string()
        } else {
            serde_json::to_string(item["input"].as_str().expect("custom input")).unwrap()
        };
        let name = serde_json::to_string(name).unwrap();
        let input = format!(
            "// @exec: {{\"yield_time_ms\":10000}}\nconst tool = ALL_TOOLS.find(t => t.name.endsWith({name}));\n\
             if (!tool) throw new Error('Required tool is unavailable');\n\
             const result = await tools[tool.name]({argument});\n\
             text(result?.output ?? result);"
        );
        *item = serde_json::json!({"type":"custom_tool_call","name":"exec",
            "call_id":item["call_id"],"input":input});
        frame
    }).collect()
}
