// SPDX-License-Identifier: Apache-2.0

//! Finite, secret-safe classifications for the embedded Core failure boundary.

use codex_protocol::protocol::CodexErrorInfo;
use serde::{Deserialize, Deserializer, Serialize, de::Error as _};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CodexFailureStage {
    EventPoll,
    EventDecode,
    CoreEvent,
    TurnComplete,
    SessionCreate,
    SessionResume,
    TurnSubmit,
    AdapterOperation,
}

impl CodexFailureStage {
    const fn as_str(self) -> &'static str {
        match self {
            Self::EventPoll => "event_poll",
            Self::EventDecode => "event_decode",
            Self::CoreEvent => "core_event",
            Self::TurnComplete => "turn_complete",
            Self::SessionCreate => "session_create",
            Self::SessionResume => "session_resume",
            Self::TurnSubmit => "turn_submit",
            Self::AdapterOperation => "adapter_operation",
        }
    }
}

/// A bounded classification. Original error text is never accepted or retained.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CodexFailureDiagnostic {
    stage: CodexFailureStage,
    #[serde(deserialize_with = "deserialize_cause")]
    cause: String,
}

impl CodexFailureDiagnostic {
    pub(crate) fn new(stage: CodexFailureStage, cause: &'static str) -> Self {
        Self {
            stage,
            cause: if allowed_cause(cause) {
                cause
            } else {
                "KERNEL_OPERATION_FAILED"
            }
            .to_owned(),
        }
    }

    pub(crate) fn kernel(stage: CodexFailureStage, code: &'static str) -> Self {
        Self::new(stage, code)
    }

    pub(crate) fn core(stage: CodexFailureStage, info: Option<&CodexErrorInfo>) -> Self {
        let cause = match info {
            Some(CodexErrorInfo::ContextWindowExceeded) => "CORE_CONTEXT_WINDOW_EXCEEDED",
            Some(CodexErrorInfo::SessionBudgetExceeded) => "CORE_SESSION_BUDGET_EXCEEDED",
            Some(CodexErrorInfo::UsageLimitExceeded) => "CORE_USAGE_LIMIT_EXCEEDED",
            Some(CodexErrorInfo::ServerOverloaded) => "CORE_SERVER_OVERLOADED",
            Some(CodexErrorInfo::CyberPolicy) => "CORE_CYBER_POLICY",
            Some(CodexErrorInfo::MisalignmentPolicyViolation) => {
                "CORE_MISALIGNMENT_POLICY_VIOLATION"
            }
            Some(CodexErrorInfo::HttpConnectionFailed { .. }) => "CORE_HTTP_CONNECTION_FAILED",
            Some(CodexErrorInfo::ResponseStreamConnectionFailed { .. }) => {
                "CORE_RESPONSE_STREAM_CONNECTION_FAILED"
            }
            Some(CodexErrorInfo::InternalServerError) => "CORE_INTERNAL_SERVER_ERROR",
            Some(CodexErrorInfo::Unauthorized) => "CORE_UNAUTHORIZED",
            Some(CodexErrorInfo::BadRequest) => "CORE_BAD_REQUEST",
            Some(CodexErrorInfo::SandboxError) => "CORE_SANDBOX_ERROR",
            Some(CodexErrorInfo::ResponseStreamDisconnected { .. }) => {
                "CORE_RESPONSE_STREAM_DISCONNECTED"
            }
            Some(CodexErrorInfo::ResponseTooManyFailedAttempts { .. }) => {
                "CORE_RESPONSE_TOO_MANY_FAILED_ATTEMPTS"
            }
            Some(CodexErrorInfo::ActiveTurnNotSteerable { .. }) => "CORE_ACTIVE_TURN_NOT_STEERABLE",
            Some(CodexErrorInfo::ThreadRollbackFailed) => "CORE_THREAD_ROLLBACK_FAILED",
            Some(CodexErrorInfo::Other) => "CORE_OTHER",
            None => "CORE_ERROR_UNCLASSIFIED",
        };
        Self::new(stage, cause)
    }

    /// Adds only the retained constant classification to an existing safe message.
    #[must_use]
    pub fn append_to(&self, message: &str) -> String {
        let suffix = format!("[stage={}; cause={}]", self.stage.as_str(), self.cause);
        if message.ends_with(&suffix) {
            message.to_owned()
        } else {
            format!("{message} {suffix}")
        }
    }
}

fn deserialize_cause<'de, D: Deserializer<'de>>(deserializer: D) -> Result<String, D::Error> {
    let cause = String::deserialize(deserializer)?;
    if allowed_cause(&cause) {
        Ok(cause)
    } else {
        Err(D::Error::custom(
            "unknown embedded Core failure classification",
        ))
    }
}

const ALLOWED_CAUSES: &[&str] = &[
    "CODEX_DATA_PATH_INVALID",
    "HELPER_PATH_INVALID",
    "MODEL_PROVIDER_INVALID",
    "MODEL_ID_INVALID",
    "MODEL_GATEWAY_ROUTE_INVALID",
    "MODEL_GATEWAY_CAPABILITY_INVALID",
    "EXECUTION_ENVELOPE_VERSION_INVALID",
    "EXECUTION_ENVELOPE_DIGEST_INVALID",
    "CODEX_DATA_ROOT_UNAVAILABLE",
    "KERNEL_HOME_UNAVAILABLE",
    "HELPER_INSTALLATION_FAILED",
    "CODEX_STORE_OPEN_FAILED",
    "KERNEL_INITIALIZATION_FAILED",
    "APPROVAL_OWNER_RESTART_MISMATCH",
    "FUSION_PANEL_FAILED",
    "KERNEL_TURN_IDENTITY_INVALID",
    "EXECUTION_DELIVERY_REJECTED",
    "HELPER_HANDSHAKE_FAILED",
    "HELPER_HOST_IDENTITY_UNAVAILABLE",
    "HELPER_HOST_RELEASE_UNAVAILABLE",
    "HELPER_IDENTITY_FAILED",
    "HELPER_IMAGE_DIGEST_MISMATCH",
    "HELPER_IMAGE_METADATA_UNAVAILABLE",
    "HELPER_IMAGE_MODE_MISMATCH",
    "HELPER_IMAGE_NOT_REGULAR",
    "HELPER_IMAGE_PATH_UNAVAILABLE",
    "HELPER_IMAGE_READ_FAILED",
    "HELPER_PLATFORM_UNSUPPORTED",
    "HELPER_RELEASE_LAYOUT_MISMATCH",
    "HELPER_VALIDATION_LOCK_UNAVAILABLE",
    "EVENT_STREAM_CLOSED",
    "EVENT_DECODE_FAILED",
    "CORE_EVENT_STREAM_FAILED",
    "CORE_EVENT_SERIALIZATION_FAILED",
    "KERNEL_OPERATION_FAILED",
    "SUBMISSION_FAILED_UNCLASSIFIED",
    "ACTION_GATE_UNAVAILABLE",
    "APPROVAL_SUBMIT_FAILED",
    "AUTH_INITIALIZATION_FAILED",
    "CONFIG_FEATURE_FAILED",
    "CONFIG_LOAD_FAILED",
    "EMPTY_INPUT",
    "ENVIRONMENT_INITIALIZATION_FAILED",
    "EXTENSION_CONFIG_UNAVAILABLE",
    "HELPER_NOT_EXECUTABLE",
    "HELPER_NOT_FOUND",
    "HOME_CREATE_FAILED",
    "HOME_PERMISSION_FAILED",
    "INPUT_RESPONSE_SUBMIT_FAILED",
    "INSTALLATION_ID_FAILED",
    "INTERRUPT_FAILED",
    "INVALID_AGENT_CONFIG",
    "INVALID_APPROVAL_RESPONSE",
    "INVALID_EXACT_TURN",
    "INVALID_HELPER_PATH",
    "INVALID_HOME",
    "INVALID_INPUT_RESPONSE",
    "INVALID_MODEL_ROUTE",
    "INVALID_ROLE_POLICY",
    "INVALID_ROLLOUT_PATH",
    "INVALID_SHUTDOWN_TIMEOUT",
    "INVALID_WORKSPACE",
    "KERNEL_CLOSED",
    "KERNEL_PANIC",
    "ROLLOUT_UNAVAILABLE",
    "RUNTIME_PATHS_INVALID",
    "SESSION_ALREADY_REGISTERED",
    "SESSION_CREATE_FAILED",
    "SESSION_EVENT_STREAM_FAILED",
    "SESSION_FLUSH_FAILED",
    "SESSION_FORK_FAILED",
    "SESSION_NOT_FOUND",
    "SESSION_POLICY_UNAVAILABLE",
    "SESSION_RESUME_FAILED",
    "SESSION_SHUTDOWN_FAILED",
    "SESSION_SHUTDOWN_TIMEOUT",
    "TURN_HISTORY_LOOKUP_FAILED",
    "TURN_RECONCILE_SUBMIT_FAILED",
    "TURN_STEER_FAILED",
    "TURN_SUBMIT_FAILED",
    "CORE_CONTEXT_WINDOW_EXCEEDED",
    "CORE_SESSION_BUDGET_EXCEEDED",
    "CORE_USAGE_LIMIT_EXCEEDED",
    "CORE_SERVER_OVERLOADED",
    "CORE_CYBER_POLICY",
    "CORE_MISALIGNMENT_POLICY_VIOLATION",
    "CORE_HTTP_CONNECTION_FAILED",
    "CORE_RESPONSE_STREAM_CONNECTION_FAILED",
    "CORE_INTERNAL_SERVER_ERROR",
    "CORE_UNAUTHORIZED",
    "CORE_BAD_REQUEST",
    "CORE_SANDBOX_ERROR",
    "CORE_RESPONSE_STREAM_DISCONNECTED",
    "CORE_RESPONSE_TOO_MANY_FAILED_ATTEMPTS",
    "CORE_ACTIVE_TURN_NOT_STEERABLE",
    "CORE_THREAD_ROLLBACK_FAILED",
    "CORE_OTHER",
    "CORE_ERROR_UNCLASSIFIED",
    "ADAPTER_INVALID_CONFIGURATION",
    "ADAPTER_AUTHORITY",
    "ADAPTER_CONFLICT",
    "ADAPTER_DURABLE_STATE",
    "ADAPTER_MODEL_BRIDGE",
    "ADAPTER_RESTART",
    "ADAPTER_UNKNOWN_THREAD",
];

fn allowed_cause(cause: &str) -> bool {
    ALLOWED_CAUSES.contains(&cause)
}

#[cfg(test)]
mod tests {
    use super::{CodexFailureDiagnostic, CodexFailureStage};

    #[test]
    fn retained_classification_rejects_unknown_cause_and_detail_fields() {
        for value in [
            serde_json::json!({"stage":"event_poll","cause":"private-token"}),
            serde_json::json!({"stage":"event_poll","cause":"KERNEL_CLOSED","message":"private-token"}),
            serde_json::json!({"stage":"private-token","cause":"KERNEL_CLOSED"}),
        ] {
            assert!(serde_json::from_value::<CodexFailureDiagnostic>(value).is_err());
        }
        let diagnostic =
            CodexFailureDiagnostic::kernel(CodexFailureStage::EventPoll, "KERNEL_CLOSED");
        let encoded = serde_json::to_value(&diagnostic).unwrap();
        assert_eq!(
            encoded,
            serde_json::json!({"stage":"event_poll","cause":"KERNEL_CLOSED"})
        );
        assert_eq!(
            serde_json::from_value::<CodexFailureDiagnostic>(encoded).unwrap(),
            diagnostic
        );
    }

    #[test]
    fn safe_message_append_is_exact_and_idempotent() {
        let diagnostic =
            CodexFailureDiagnostic::new(CodexFailureStage::EventDecode, "EVENT_DECODE_FAILED");
        let expected =
            "embedded Codex infrastructure failed [stage=event_decode; cause=EVENT_DECODE_FAILED]";
        assert_eq!(
            diagnostic.append_to("embedded Codex infrastructure failed"),
            expected
        );
        assert_eq!(diagnostic.append_to(expected), expected);
    }
    #[test]
    fn unknown_kernel_code_never_becomes_public_detail() {
        let diagnostic = CodexFailureDiagnostic::kernel(
            CodexFailureStage::EventPoll,
            "private-token=/private/z7xn-credential",
        );
        assert_eq!(
            diagnostic.append_to("embedded Codex infrastructure failed"),
            "embedded Codex infrastructure failed [stage=event_poll; cause=KERNEL_OPERATION_FAILED]",
        );
    }
}
