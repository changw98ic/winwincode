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

/// Exact raw or handler-parsed payload presented at the corresponding admission point.
#[derive(Clone, Eq, PartialEq)]
pub enum ToolCallGatePayload {
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
            Self::Function { .. } => "Function(<private>)",
            Self::ToolSearch { .. } => "ToolSearch(<private>)",
            Self::Custom { .. } => "Custom(<private>)",
            Self::Shell { .. } => "Shell(<private>)",
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

/// Typed host admission boundary invoked before every Codex tool handler.
pub trait ToolCallGate: Send + Sync {
    fn authorize(
        &self,
        request: ToolCallGateRequest,
    ) -> BoxFuture<'static, Result<(), ToolCallGateRejection>>;

    fn revalidate(
        &self,
        request: ToolCallGateRequest,
    ) -> BoxFuture<'static, Result<(), ToolCallGateRejection>>;
}

#[cfg(test)]
mod tests {
    use super::{ToolCallGateFileChange, ToolCallGatePayload, ToolCallGateRequest};

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
