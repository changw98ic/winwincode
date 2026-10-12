use std::fmt;
use std::sync::Arc;

use futures::future::BoxFuture;

/// Host-owned request observed immediately before a Codex tool handler runs.
#[derive(Clone, Eq, PartialEq)]
pub struct ToolCallGateRequest {
    pub thread_id: String,
    pub turn_id: String,
    pub call_id: String,
    pub namespace: Option<String>,
    pub tool_name: String,
    pub payload: ToolCallGatePayload,
}

/// Receipt identity for reading an accepted historical result. This grants no
/// permission to execute the operation or revive its external handles.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
pub struct ToolResultReadRequest {
    pub thread_id: String,
    pub turn_id: String,
    pub call_id: String,
    pub request_sequence: i64,
    pub logical_id: String,
    pub attempt_id: String,
    pub operation_digest: String,
    pub revision: i64,
}

/// Exact raw or handler-parsed payload presented at the corresponding admission point.
#[derive(Clone, Eq, PartialEq)]
pub enum ToolCallGatePayload {
    /// Orchestration certified by the registered Core runtime. Nested I/O must
    /// obtain its own host authorization. The input retains kind and raw value.
    CoreControl {
        input: String,
    },
    Function {
        arguments: String,
    },
    ToolSearch {
        arguments_json: String,
    },
    Custom {
        input: String,
    },
    Shell {
        program: String,
        args: Vec<String>,
        working_directory: String,
    },
    FileRead {
        path: String,
    },
    McpResource {
        server: String,
        method: String,
        arguments: String,
    },
    ProcessInteraction {
        process_id: i32,
        origin_call_id: String,
        input: String,
    },
    Files {
        changes: Vec<ToolCallGateFileChange>,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToolCallGateFileOperation {
    Create,
    Write,
    Delete,
}

#[derive(Clone, Eq, PartialEq)]
pub struct ToolCallGateFileChange {
    pub operation: ToolCallGateFileOperation,
    pub path: String,
    pub move_path: Option<String>,
}

impl fmt::Debug for ToolCallGateFileChange {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ToolCallGateFileChange")
            .field("operation", &self.operation)
            .field("path", &"<private>")
            .field("move_path", &self.move_path.as_ref().map(|_| "<private>"))
            .finish()
    }
}

impl fmt::Debug for ToolCallGateRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ToolCallGateRequest")
            .field("thread_id", &self.thread_id)
            .field("turn_id", &self.turn_id)
            .field("call_id", &self.call_id)
            .field("namespace", &self.namespace)
            .field("tool_name", &self.tool_name)
            .field("payload", &self.payload)
            .finish()
    }
}

impl fmt::Debug for ToolCallGatePayload {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::CoreControl { .. } => "CoreControl(<private>)",
            Self::Function { .. } => "Function(<private>)",
            Self::ToolSearch { .. } => "ToolSearch(<private>)",
            Self::Custom { .. } => "Custom(<private>)",
            Self::Shell { .. } => "Shell(<private>)",
            Self::FileRead { .. } => "FileRead(<private>)",
            Self::McpResource { .. } => "McpResource(<private>)",
            Self::ProcessInteraction { .. } => "ProcessInteraction(<private>)",
            Self::Files { .. } => "Files(<private>)",
        })
    }
}

/// Stable host rejection returned before the tool handler can perform a side effect.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolCallGateRejection {
    code: &'static str,
    message: &'static str,
}

impl ToolCallGateRejection {
    pub const fn new(code: &'static str, message: &'static str) -> Self {
        Self { code, message }
    }

    pub const fn code(&self) -> &'static str {
        self.code
    }
}

impl fmt::Display for ToolCallGateRejection {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for ToolCallGateRejection {}

/// Host authorization retained across admission and the final pre-execution
/// revalidation.  An executable authorization is deliberately an absolute
/// path plus an opaque host identity: runtimes must execute that path instead
/// of resolving the model-provided program through `PATH` again.
#[derive(Clone, Default, Eq, PartialEq)]
pub struct ToolCallGateAuthorization {
    request_binding: String,
    executable: Option<ToolCallGateExecutableAuthorization>,
}

impl fmt::Debug for ToolCallGateAuthorization {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ToolCallGateAuthorization")
            .field("request_binding", &"<private>")
            .field("executable", &self.executable.as_ref().map(|_| "<private>"))
            .finish()
    }
}

impl ToolCallGateAuthorization {
    pub fn new(
        request_binding: String,
        executable: Option<ToolCallGateExecutableAuthorization>,
    ) -> Self {
        Self {
            request_binding,
            executable,
        }
    }

    pub fn request_binding(&self) -> &str {
        &self.request_binding
    }

    pub fn executable(&self) -> Option<&ToolCallGateExecutableAuthorization> {
        self.executable.as_ref()
    }
}

/// Exact executable selected by the host action authority.
#[derive(Clone, Eq, PartialEq)]
pub struct ToolCallGateExecutableAuthorization {
    canonical_absolute_path: String,
    arguments: Vec<String>,
    identity: String,
}

impl fmt::Debug for ToolCallGateExecutableAuthorization {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ToolCallGateExecutableAuthorization")
            .field("canonical_absolute_path", &"<private>")
            .field("arguments", &"<private>")
            .field("identity", &"<private>")
            .finish()
    }
}

impl ToolCallGateExecutableAuthorization {
    pub fn new(canonical_absolute_path: String, arguments: Vec<String>, identity: String) -> Self {
        Self {
            canonical_absolute_path,
            arguments,
            identity,
        }
    }

    pub fn canonical_absolute_path(&self) -> &str {
        &self.canonical_absolute_path
    }

    pub fn identity(&self) -> &str {
        &self.identity
    }

    pub fn arguments(&self) -> &[String] {
        &self.arguments
    }
}

/// Typed host admission boundary invoked before every Codex tool handler.
pub trait ToolCallGate: Send + Sync {
    /// Freeze trusted dependency, policy and authority identities. Unknown
    /// adapters execute normally and receive no result-sharing permission.
    fn freeze_tool_input(
        &self,
        _context: crate::ToolInputContext,
    ) -> BoxFuture<'static, Option<crate::ToolDependencySnapshot>> {
        Box::pin(async { None })
    }

    /// Verify the real execution source from adapter-owned evidence. A missing
    /// proof cannot make an output reusable or advance diagnostic progress.
    fn verify_tool_input(
        &self,
        _request: crate::ToolInputProofRequest,
    ) -> BoxFuture<'static, Option<crate::ToolInputProof>> {
        Box::pin(async { None })
    }

    /// Authorize access to this exact stored receipt under current authority.
    /// Hosts must implement this independently of operation execution permission.
    fn authorize_result_read(
        &self,
        _request: ToolResultReadRequest,
    ) -> BoxFuture<'static, Result<ToolCallGateAuthorization, ToolCallGateRejection>> {
        Box::pin(async {
            Err(ToolCallGateRejection::new(
                "RESULT_READ_UNAVAILABLE",
                "stored result read authority is unavailable",
            ))
        })
    }

    /// Recheck the receipt binding and current authority immediately before delivery.
    fn revalidate_result_read(
        &self,
        _request: ToolResultReadRequest,
        _authorization: ToolCallGateAuthorization,
    ) -> BoxFuture<'static, Result<(), ToolCallGateRejection>> {
        Box::pin(async {
            Err(ToolCallGateRejection::new(
                "RESULT_READ_UNAVAILABLE",
                "stored result read authority is unavailable",
            ))
        })
    }

    fn authorize(
        &self,
        request: ToolCallGateRequest,
    ) -> BoxFuture<'static, Result<ToolCallGateAuthorization, ToolCallGateRejection>>;

    fn revalidate(
        &self,
        request: ToolCallGateRequest,
        authorization: ToolCallGateAuthorization,
    ) -> BoxFuture<'static, Result<(), ToolCallGateRejection>>;
}

#[cfg(test)]
mod tests {
    use super::ToolCallGateAuthorization;
    use super::ToolCallGateExecutableAuthorization;
    use super::ToolCallGateFileChange;
    use super::ToolCallGatePayload;
    use super::ToolCallGateRequest;

    #[test]
    fn debug_output_redacts_exact_tool_payload() {
        let request = ToolCallGateRequest {
            thread_id: "thread".to_string(),
            turn_id: "turn".to_string(),
            call_id: "call".to_string(),
            namespace: Some("functions".to_string()),
            tool_name: "shell_command".to_string(),
            payload: ToolCallGatePayload::Function {
                arguments: "TOKEN=TOKEN_VALUE PAYLOAD=PAYLOAD_VALUE".to_string(),
            },
        };
        let rendered = format!("{request:?}");
        assert!(!rendered.contains("TOKEN_VALUE"));
        assert!(!rendered.contains("PAYLOAD_VALUE"));

        let path = ToolCallGateFileChange {
            operation: super::ToolCallGateFileOperation::Write,
            path: "/private/TOKEN_VALUE".to_string(),
            move_path: Some("/private/PAYLOAD_VALUE".to_string()),
        };
        let rendered = format!("{path:?}");
        assert!(!rendered.contains("TOKEN_VALUE"));
        assert!(!rendered.contains("PAYLOAD_VALUE"));
    }

    #[test]
    fn debug_output_redacts_action_authorization() {
        let authorization = ToolCallGateAuthorization::new(
            "REQUEST_BINDING_VALUE".to_owned(),
            Some(ToolCallGateExecutableAuthorization::new(
                "/private/executable".to_owned(),
                vec!["SECRET_ARGUMENT".to_owned()],
                "IDENTITY_DIGEST_VALUE".to_owned(),
            )),
        );
        let rendered = format!("{authorization:?}");
        for secret in [
            "REQUEST_BINDING_VALUE",
            "/private/executable",
            "SECRET_ARGUMENT",
            "IDENTITY_DIGEST_VALUE",
        ] {
            assert!(!rendered.contains(secret), "{secret} leaked in {rendered}");
        }

        let executable = authorization.executable().expect("authorized executable");
        let rendered = format!("{executable:?}");
        for secret in [
            "/private/executable",
            "SECRET_ARGUMENT",
            "IDENTITY_DIGEST_VALUE",
        ] {
            assert!(!rendered.contains(secret), "{secret} leaked in {rendered}");
        }
    }
}

/// Thread attachment installed by a host-owned [`ToolCallGate`].
#[derive(Clone)]
pub struct ToolCallGateAttachment(Arc<dyn ToolCallGate>);

impl ToolCallGateAttachment {
    pub fn new(gate: Arc<dyn ToolCallGate>) -> Self {
        Self(gate)
    }

    pub fn gate(&self) -> Arc<dyn ToolCallGate> {
        Arc::clone(&self.0)
    }
}
