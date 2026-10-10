// SPDX-License-Identifier: Apache-2.0

//! An isolated subprocess may select an existing Core collaboration mode. This
//! hook is compiled only by `WinWinCode`'s explicit test-support feature.

pub(super) fn apply(
    mut request: codex_core_api::TurnInputRequest,
) -> codex_core_api::TurnInputRequest {
    let mode = match std::env::var("WWC_MECHANISM_CORE_MODE").as_deref() {
        Ok("Plan") => Some(codex_protocol::config_types::ModeKind::Plan),
        Ok("Default") | Err(_) => None,
        Ok(_) => panic!("unknown isolated mechanism collaboration mode"),
    };
    if let Some(mode) = mode {
        request.thread_settings.collaboration_mode =
            Some(codex_protocol::config_types::CollaborationMode {
                mode,
                settings: codex_protocol::config_types::Settings {
                    model: "loopback-model".into(),
                    reasoning_effort: None,
                    developer_instructions: None,
                },
            });
    }
    request
}
