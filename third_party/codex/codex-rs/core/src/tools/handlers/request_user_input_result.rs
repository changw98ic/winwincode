// SPDX-License-Identifier: Apache-2.0

use crate::function_tool::FunctionCallError;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolOutput;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::control_input_recovery::error;
use crate::tools::control_input_recovery::read_request;
use crate::tools::control_input_recovery::result_ref;
use crate::tools::execution_facts::digest;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;
use codex_protocol::models::ResponseInputItem;
use codex_state::ControlInputRecovery;
use codex_state::ControlInputResultPage;
use codex_tools::JsonSchema;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolName;
use codex_tools::ToolSpec;
use serde::Deserialize;
use serde_json::Value;
use serde_json::json;
use std::collections::BTreeMap;

const NAME: &str = "request_user_input_result";
const MAX_PAGE_BYTES: usize = 8 * 1024;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Args {
    result_ref: String,
    cursor: String,
}

struct PrivateInputPage(String);
impl ToolOutput for PrivateInputPage {
    fn log_preview(&self) -> String {
        "private recovered input result page".into()
    }
    fn success_for_logging(&self) -> bool {
        true
    }
    fn to_response_item(&self, call_id: &str, payload: &ToolPayload) -> ResponseInputItem {
        FunctionToolOutput::from_text(self.0.clone(), Some(true)).to_response_item(call_id, payload)
    }
    fn code_mode_result(&self, _: &ToolPayload) -> Value {
        match serde_json::from_str(&self.0) {
            Ok(value) => value,
            Err(_) => json!({"type":"tool_recovery","schema_version":1,
                "state":"control_input_result_invalid","success":false}),
        }
    }
}

pub struct RequestUserInputResultHandler;
impl ToolExecutor<ToolInvocation> for RequestUserInputResultHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(NAME)
    }
    fn spec(&self) -> ToolSpec {
        ToolSpec::Function(ResponsesApiTool { name: NAME.into(),
            description: "Read one private page of a consumed input answer after a CodeMode interruption. Use the exact resultRef and cursor from tool_recovery, then nextCursor until null. Concatenate content to reconstruct the complete JSON response. Each serialized page is at most 8 KiB. This is read-only and never resumes the old script.".into(),
            strict:false, defer_loading:None,
            parameters:JsonSchema::object(BTreeMap::from([
                ("resultRef".into(),JsonSchema::string(Some("Exact private result reference from tool_recovery.".into()))),
                ("cursor".into(),JsonSchema::string(Some("Use 0 for the first page, otherwise the exact nextCursor.".into()))),
            ]),Some(vec!["resultRef".into(),"cursor".into()]),Some(false.into())), output_schema:None })
    }
    fn handle(&self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(async move {
            let ToolPayload::Function { arguments } = &invocation.payload else {
                return Err(error("control_input_result_invalid"));
            };
            let args: Args = super::parse_arguments(arguments)?;
            let facts = crate::tools::ExecutionFacts::for_session(&invocation.session);
            let store = facts
                .store
                .get()
                .or(invocation.session.services.state_db.as_ref())
                .ok_or_else(|| error("control_input_result_unavailable"))?;
            Ok(boxed_tool_output(PrivateInputPage(
                read_page(store, &invocation, &args).await?,
            )))
        })
    }
}

async fn read_page(
    store: &codex_state::StateRuntime,
    invocation: &ToolInvocation,
    args: &Args,
) -> Result<String, FunctionCallError> {
    if invocation.turn.session_source.is_non_root_agent()
        || args.result_ref.len() > 128
        || args.cursor.len() > 128
    {
        return Err(error("control_input_result_denied"));
    }
    let thread = invocation.session.thread_id.to_string();
    let header = store
        .control_input_recovery_header(&thread, &args.result_ref)
        .await
        .map_err(crate::tools::execution_facts::storage_error)?
        .ok_or_else(|| error("control_input_result_unavailable"))?;
    if header.parent.request.turn_id != invocation.turn.sub_id
        || header.input.request.turn_id != invocation.turn.sub_id
        || args.result_ref != result_ref(&header.parent, &header.input)
        || store
            .tool_execution_fact(header.parent.request_sequence)
            .await
            .map_err(crate::tools::execution_facts::storage_error)?
            != header.parent
        || store
            .tool_execution_fact(header.input.request_sequence)
            .await
            .map_err(crate::tools::execution_facts::storage_error)?
            != header.input
    {
        return Err(error("control_input_result_scope_expired"));
    }
    let attachment = invocation
        .session
        .services
        .thread_extension_data
        .get::<crate::ToolCallGateAttachment>()
        .ok_or_else(|| error("result_read_authority_unavailable"))?;
    let gate = attachment.gate();
    let parent = read_request(
        &header.parent,
        header.input.request.parent_call_id.as_deref().unwrap_or(""),
    )?;
    let input = read_request(&header.input, &header.input.request.logical_id)?;
    let parent_auth = gate
        .authorize_result_read(parent.clone())
        .await
        .map_err(|_| error("result_read_denied"))?;
    let input_auth = gate
        .authorize_result_read(input.clone())
        .await
        .map_err(|_| error("result_read_denied"))?;
    let page = store
        .control_input_result_page(&thread, &args.result_ref, &args.cursor)
        .await
        .map_err(crate::tools::execution_facts::storage_error)?
        .ok_or_else(|| error("control_input_cursor_invalid"))?;
    let value: Value =
        serde_json::from_str(&page.page_json).map_err(|_| error("control_input_result_invalid"))?;
    if page.page_json.len() > MAX_PAGE_BYTES
        || digest(json!(page.page_json)) != page.page_digest
        || value["resultRef"] != header.result_ref
        || value["responseDigest"] != header.response_digest
        || value["cursor"] != args.cursor
        || value["totalBytes"] != header.total_bytes
        || store
            .control_input_recovery_header(&thread, &args.result_ref)
            .await
            .map_err(crate::tools::execution_facts::storage_error)?
            .as_ref()
            != Some(&header)
        || store
            .tool_execution_fact(header.parent.request_sequence)
            .await
            .map_err(crate::tools::execution_facts::storage_error)?
            != header.parent
        || store
            .tool_execution_fact(header.input.request_sequence)
            .await
            .map_err(crate::tools::execution_facts::storage_error)?
            != header.input
    {
        return Err(error("control_input_result_scope_expired"));
    }
    gate.revalidate_result_read(parent, parent_auth)
        .await
        .map_err(|_| error("result_read_denied"))?;
    gate.revalidate_result_read(input, input_auth)
        .await
        .map_err(|_| error("result_read_denied"))?;
    Ok(page.page_json)
}

pub(in crate::tools) fn prepare_pages(
    record: &ControlInputRecovery,
) -> Result<Vec<ControlInputResultPage>, FunctionCallError> {
    let mut supplied = "0".to_owned();
    let mut pages = Vec::new();
    loop {
        let page_json = page(record, &supplied)?;
        let value: Value =
            serde_json::from_str(&page_json).map_err(|_| error("control_input_result_invalid"))?;
        let next = value["nextCursor"].as_str().map(ToOwned::to_owned);
        let page_digest = digest(json!(page_json));
        pages.push(ControlInputResultPage {
            cursor: supplied,
            page_json,
            page_digest,
        });
        match next {
            Some(next) => supplied = next,
            None => break,
        }
    }
    Ok(pages)
}
impl CoreToolRuntime for RequestUserInputResultHandler {
    fn authorization_policy(&self) -> crate::tools::authorization::AuthorizationPolicy {
        crate::tools::authorization::AuthorizationPolicy::CoreControl
    }
    fn is_builtin_control_tool(&self) -> bool {
        true
    }
}

fn cursor(record: &ControlInputRecovery, offset: usize) -> String {
    format!(
        "{offset}:{}",
        digest(json!([record.result_ref, record.response_digest, offset]))
    )
}

fn page(record: &ControlInputRecovery, supplied: &str) -> Result<String, FunctionCallError> {
    if supplied.len() > 128 {
        return Err(error("control_input_cursor_invalid"));
    }
    let offset = if supplied == "0" {
        0
    } else {
        let offset = supplied
            .split_once(':')
            .and_then(|(offset, _)| offset.parse::<usize>().ok())
            .ok_or_else(|| error("control_input_cursor_invalid"))?;
        if supplied != cursor(record, offset) {
            return Err(error("control_input_cursor_invalid"));
        }
        offset
    };
    if offset > record.response_json.len() || !record.response_json.is_char_boundary(offset) {
        return Err(error("control_input_cursor_invalid"));
    }
    let encode = |end: usize| {
        json!({"resultRef":record.result_ref,"responseDigest":record.response_digest,
        "cursor":supplied,"nextCursor":(end<record.response_json.len()).then(||cursor(record,end)),
        "totalBytes":record.response_json.len(),"content":&record.response_json[offset..end]})
        .to_string()
    };
    // Bound discovery to one page, then binary-search the escaped serialized
    // budget. Never repeatedly encode the full answer or scan preceding pages.
    let ends = record.response_json[offset..]
        .char_indices()
        .map(|(index, ch)| offset + index + ch.len_utf8())
        .take_while(|end| end - offset <= MAX_PAGE_BYTES)
        .collect::<Vec<_>>();
    let mut low = 0;
    let mut high = ends.len();
    while low < high {
        let middle = low + (high - low).div_ceil(2);
        if encode(ends[middle - 1]).len() <= MAX_PAGE_BYTES {
            low = middle;
        } else {
            high = middle - 1;
        }
    }
    let end = if low == 0 { offset } else { ends[low - 1] };
    let output = encode(end);
    if output.len() > MAX_PAGE_BYTES || (end == offset && offset < record.response_json.len()) {
        return Err(error("control_input_page_limit"));
    }
    Ok(output)
}

#[cfg(test)]
#[path = "request_user_input_result_tests.rs"]
mod tests;
