// SPDX-License-Identifier: Apache-2.0

//! Auxiliary source facts for the first real expired-action rejection per run.
//! These facts grant no authority and never replace execution or usage receipts.

use std::collections::HashMap;

use serde::Serialize;
use sha2::{Digest as _, Sha256};
use winwincode_domain::Instant;
use winwincode_kernel::{KernelActionPayload, KernelActionRequest};

use crate::model_bridge::ModelRunBinding;
use crate::model_port_client::ModelLeaseAuthority;

const MAX_DIAGNOSTIC_RUNS: usize = 1_024;

#[derive(Default)]
pub(super) struct ActionAuthorityDiagnostics {
    entries: HashMap<String, String>,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum ActionAuthorityScope {
    ExecutionLease,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum ActionAuthorityCause {
    LeaseExpired,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ActionAuthorityDiagnostic<'a> {
    scope: ActionAuthorityScope,
    cause: ActionAuthorityCause,
    authority: &'a ModelLeaseAuthority,
    observed_at: &'a Instant,
    kernel_thread_digest: String,
    core_turn_digest: String,
    operation_digest: String,
    tool_name_digest: String,
    input_binding: Option<String>,
}

impl ActionAuthorityDiagnostics {
    pub(super) fn record_expiry(
        &mut self,
        binding: &ModelRunBinding,
        request: &KernelActionRequest,
        now: &Instant,
    ) -> Option<String> {
        if self.entries.contains_key(&binding.run_key) || self.entries.len() >= MAX_DIAGNOSTIC_RUNS
        {
            return None;
        }
        let facts = ActionAuthorityDiagnostic {
            scope: ActionAuthorityScope::ExecutionLease,
            cause: ActionAuthorityCause::LeaseExpired,
            authority: &binding.authority,
            observed_at: now,
            kernel_thread_digest: digest(&request.session_id),
            core_turn_digest: digest(&request.turn_id),
            operation_digest: digest(&request.operation_id),
            tool_name_digest: digest(&request.tool_name),
            input_binding: match &request.payload {
                KernelActionPayload::CoreControl { input } => Some(digest(input)),
                _ => None,
            },
        };
        let bytes = serde_json::to_string(&facts).ok()?;
        self.entries.insert(binding.run_key.clone(), bytes.clone());
        Some(bytes)
    }

    #[cfg(test)]
    pub(super) fn entries(&self) -> &HashMap<String, String> {
        &self.entries
    }
}

fn digest(value: &str) -> String {
    format!("{:x}", Sha256::digest(value.as_bytes()))
}
