// SPDX-License-Identifier: Apache-2.0

//! Isolated offline fault barriers. This module exists only with the explicit
//! mechanism-test-support Cargo feature and never changes a normal build.

use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use std::path::PathBuf;

fn directory() -> Option<PathBuf> {
    std::env::var_os("WWC_MECHANISM_INTERACTION_BARRIER").map(PathBuf::from)
}

pub(crate) fn record_request(event: &EventMsg) {
    let Some(directory) = directory() else {
        return;
    };
    std::fs::create_dir_all(&directory)
        .unwrap_or_else(|error| panic!("isolated mechanism barrier directory: {error:?}"));
    let filename = match event {
        EventMsg::RequestUserInput(_) => "input.request.event.json",
        EventMsg::ExecApprovalRequest(_) => "approval.request.event.json",
        _ => return,
    };
    std::fs::write(
        directory.join(filename),
        serde_json::to_vec(event).unwrap_or_else(|error| panic!("real Core event JSON: {error:?}")),
    )
    .unwrap_or_else(|error| panic!("isolated Core event witness: {error:?}"));
}

pub(crate) async fn before_response_dispatch(op: &Op) {
    let kind = match op {
        Op::UserInputAnswer { .. } => "input",
        Op::ExecApproval { .. } | Op::PatchApproval { .. } => "approval",
        _ => return,
    };
    let Some(directory) = directory() else {
        return;
    };
    if !directory.join(format!("{kind}.dispatch.arm")).exists() {
        return;
    }
    std::fs::write(
        directory.join(format!("{kind}.dispatch.paused")),
        format!("{op:?}"),
    )
    .unwrap_or_else(|error| panic!("queued Core operation witness: {error:?}"));
    let started = std::time::Instant::now();
    while !directory.join(format!("{kind}.dispatch.release")).exists() {
        assert!(
            started.elapsed() < std::time::Duration::from_secs(15),
            "isolated response barrier was not released"
        );
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
    }
}

pub(crate) fn record_waiter_consumed(kind: &str, identity: &str) {
    let Some(directory) = directory() else {
        return;
    };
    std::fs::write(
        directory.join(format!("{kind}.waiter.consumed")),
        identity.as_bytes(),
    )
    .unwrap_or_else(|error| panic!("real waiter consumption witness: {error:?}"));
}

pub(crate) fn record_response_delivery(kind: &str, state: &str) {
    let Some(directory) = directory() else {
        return;
    };
    std::fs::write(directory.join(format!("{kind}.notify.{state}")), state)
        .unwrap_or_else(|error| panic!("real response dispatch witness: {error:?}"));
}

#[cfg(test)]
#[path = "mechanism_interaction_test_barrier_tests.rs"]
mod tests;
