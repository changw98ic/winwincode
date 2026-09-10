// SPDX-License-Identifier: Apache-2.0

//! Provider-neutral streaming conversion for the central Gateway.
//!
//! Provider adapters emit typed wire events. The converter validates their
//! lifecycle, coalesces content blocks so Provider chunk boundaries do not
//! change the canonical result, assigns stable tool identities and stream
//! sequences, and produces the embedded kernel's canonical `ModelPort` JSON.
//! No old UI message format is accepted.

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt::{self, Write as _},
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::Serialize;
use sha2::{Digest, Sha256};
use winwincode_domain::Sha256Digest;
use winwincode_execution_port::generated::EncodedPayload;

use crate::{CredentialLeakGate, CredentialOutputBoundary, ProviderGatewayOpenReceipt};

const MAX_OPEN_BLOCKS: usize = 128;
const MAX_STREAM_BLOCKS: usize = 4_096;
const MAX_CONTENT_BYTES: usize = 16 * 1024 * 1024;
const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

/// Provider tool category mapped to the kernel's canonical response item.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderToolKind {
    Function,
    Custom,
}

/// Canonical Provider-neutral identity of one callable tool.
///
/// Provider adapters may expose a bounded alias on their wire protocol, but
/// the alias is never parsed back into this identity. The adapter must retain
/// an explicit alias-to-identity binding for the whole exchange.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderToolIdentity {
    kind: ProviderToolKind,
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    namespace: Option<String>,
}

impl ProviderToolIdentity {
    /// Builds one canonical identity after enforcing the Codex tool bounds.
    ///
    /// # Errors
    ///
    /// Rejects empty, overlong, or non-identifier names and namespaces.
    pub fn try_new(
        kind: ProviderToolKind,
        name: String,
        namespace: Option<String>,
    ) -> Result<Self, ProviderToolIdentityError> {
        validate_tool_component(&name, 128).map_err(|()| ProviderToolIdentityError::InvalidName)?;
        if namespace
            .as_deref()
            .is_some_and(|value| validate_tool_component(value, 64).is_err())
        {
            return Err(ProviderToolIdentityError::InvalidNamespace);
        }
        Ok(Self {
            kind,
            name,
            namespace,
        })
    }

    #[must_use]
    pub const fn kind(&self) -> ProviderToolKind {
        self.kind
    }

    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    #[must_use]
    pub fn namespace(&self) -> Option<&str> {
        self.namespace.as_deref()
    }
}

/// Stable invalid canonical tool identity category.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProviderToolIdentityError {
    InvalidName,
    InvalidNamespace,
}

impl fmt::Display for ProviderToolIdentityError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Provider tool identity is invalid")
    }
}

impl std::error::Error for ProviderToolIdentityError {}

/// Provider terminal reason which has no arbitrary diagnostic text.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProviderFinishReason {
    Stop,
    ToolCalls,
    MaxTokens,
}

/// Stable Provider stream failure categories.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProviderStreamFailureKind {
    Authentication,
    InvalidRequest,
    RateLimit,
    Quota,
    Timeout,
    Transport,
    Server,
    ContextWindowExceeded,
    Unknown,
}

/// Bounded Provider failure facts. Arbitrary Provider messages are absent.
#[derive(Clone, Eq, PartialEq)]
pub struct ProviderStreamFailure {
    kind: ProviderStreamFailureKind,
    status: Option<u16>,
    retry_after_millis: Option<u64>,
    provider_request_id: Option<String>,
}

impl ProviderStreamFailure {
    #[must_use]
    pub const fn new(kind: ProviderStreamFailureKind) -> Self {
        Self {
            kind,
            status: None,
            retry_after_millis: None,
            provider_request_id: None,
        }
    }

    #[must_use]
    pub const fn with_status(mut self, status: u16) -> Self {
        self.status = Some(status);
        self
    }

    #[must_use]
    pub const fn with_retry_after_millis(mut self, retry_after_millis: u64) -> Self {
        self.retry_after_millis = Some(retry_after_millis);
        self
    }

    #[must_use]
    pub fn with_provider_request_id(mut self, provider_request_id: String) -> Self {
        self.provider_request_id = Some(provider_request_id);
        self
    }

    #[must_use]
    pub const fn kind(&self) -> ProviderStreamFailureKind {
        self.kind
    }
}

impl fmt::Debug for ProviderStreamFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderStreamFailure")
            .field("kind", &self.kind)
            .field("status", &self.status)
            .field("retry_after_millis", &self.retry_after_millis)
            .field(
                "provider_request_id",
                &self.provider_request_id.as_ref().map(|_| "[REDACTED]"),
            )
            .finish()
    }
}

/// Provider token accounting retained until the terminal canonical event.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProviderTokenUsage {
    pub input_tokens: u64,
    pub cached_input_tokens: u64,
    pub cache_write_input_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_output_tokens: u64,
}

/// Typed wire event accepted from a selected Provider adapter.
///
/// Debug output intentionally reveals only the variant name; response content,
/// reasoning, tool arguments and Provider request identities remain hidden.
#[derive(Clone, PartialEq)]
pub enum ProviderStreamEvent {
    ResponseStarted {
        provider_response_id: String,
    },
    TextStarted {
        index: u32,
    },
    TextDelta {
        index: u32,
        delta: String,
    },
    TextEnded {
        index: u32,
    },
    ReasoningStarted {
        index: u32,
        summary_index: u32,
    },
    ReasoningSummaryDelta {
        index: u32,
        summary_index: u32,
        delta: String,
    },
    ReasoningContentDelta {
        index: u32,
        content_index: u32,
        delta: String,
    },
    ReasoningEnded {
        index: u32,
    },
    ToolCallStarted {
        index: u32,
        provider_call_id: String,
        identity: ProviderToolIdentity,
    },
    ToolCallArgumentsDelta {
        index: u32,
        provider_call_id: String,
        delta: String,
    },
    ToolCallEnded {
        index: u32,
        provider_call_id: String,
    },
    Usage(ProviderTokenUsage),
    Finished(ProviderFinishReason),
    Failed(ProviderStreamFailure),
    Cancelled,
    Disconnected,
}

impl fmt::Debug for ProviderStreamEvent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::ResponseStarted { .. } => "ProviderStreamEvent::ResponseStarted([REDACTED])",
            Self::TextStarted { .. } => "ProviderStreamEvent::TextStarted",
            Self::TextDelta { .. } => "ProviderStreamEvent::TextDelta([REDACTED])",
            Self::TextEnded { .. } => "ProviderStreamEvent::TextEnded",
            Self::ReasoningStarted { .. } => "ProviderStreamEvent::ReasoningStarted",
            Self::ReasoningSummaryDelta { .. } => {
                "ProviderStreamEvent::ReasoningSummaryDelta([REDACTED])"
            }
            Self::ReasoningContentDelta { .. } => {
                "ProviderStreamEvent::ReasoningContentDelta([REDACTED])"
            }
            Self::ReasoningEnded { .. } => "ProviderStreamEvent::ReasoningEnded",
            Self::ToolCallStarted { .. } => "ProviderStreamEvent::ToolCallStarted([REDACTED])",
            Self::ToolCallArgumentsDelta { .. } => {
                "ProviderStreamEvent::ToolCallArgumentsDelta([REDACTED])"
            }
            Self::ToolCallEnded { .. } => "ProviderStreamEvent::ToolCallEnded([REDACTED])",
            Self::Usage(_) => "ProviderStreamEvent::Usage",
            Self::Finished(_) => "ProviderStreamEvent::Finished",
            Self::Failed(_) => "ProviderStreamEvent::Failed([REDACTED])",
            Self::Cancelled => "ProviderStreamEvent::Cancelled",
            Self::Disconnected => "ProviderStreamEvent::Disconnected",
        })
    }
}

/// Stable conversion failure categories.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProviderStreamConversionErrorKind {
    InvalidEvent,
    Protocol,
    AlreadyTerminal,
    CredentialLeak,
    SizeLimit,
    SequenceOverflow,
}

/// Secret-free conversion error.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderStreamConversionError {
    kind: ProviderStreamConversionErrorKind,
    message: &'static str,
}

impl ProviderStreamConversionError {
    const fn new(kind: ProviderStreamConversionErrorKind, message: &'static str) -> Self {
        Self { kind, message }
    }

    const fn invalid() -> Self {
        Self::new(
            ProviderStreamConversionErrorKind::InvalidEvent,
            "Provider stream event is invalid",
        )
    }

    const fn protocol() -> Self {
        Self::new(
            ProviderStreamConversionErrorKind::Protocol,
            "Provider stream lifecycle is invalid",
        )
    }

    #[must_use]
    pub const fn kind(&self) -> ProviderStreamConversionErrorKind {
        self.kind
    }
}

impl fmt::Display for ProviderStreamConversionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.message)
    }
}

impl std::error::Error for ProviderStreamConversionError {}

/// One ordered, leak-checked canonical `ModelPort` message.
#[derive(Clone, Eq, PartialEq)]
pub struct CanonicalModelStreamFrame {
    sequence: u64,
    payload_json: String,
    terminal: bool,
}

impl CanonicalModelStreamFrame {
    #[must_use]
    pub const fn sequence(&self) -> u64 {
        self.sequence
    }

    /// Returns the exact JSON consumed by the embedded kernel's `ModelPort`.
    #[must_use]
    pub fn payload_json(&self) -> &str {
        &self.payload_json
    }

    /// Builds the generated opaque `ExecutionPort` payload for this event.
    #[must_use]
    pub fn encoded_payload(&self) -> EncodedPayload {
        EncodedPayload {
            content_type: "application/json".to_owned(),
            data_base64: STANDARD.encode(self.payload_json.as_bytes()),
            payload_digest: Sha256Digest(format!(
                "sha256:{:x}",
                Sha256::digest(self.payload_json.as_bytes())
            )),
        }
    }

    #[must_use]
    pub const fn is_terminal(&self) -> bool {
        self.terminal
    }
}

impl fmt::Debug for CanonicalModelStreamFrame {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CanonicalModelStreamFrame")
            .field("sequence", &self.sequence)
            .field("payload_json", &"[REDACTED]")
            .field("terminal", &self.terminal)
            .finish()
    }
}

#[derive(Clone)]
struct TextBlock {
    item_id: String,
    text: String,
}

#[derive(Clone)]
enum ReasoningSegment {
    Summary { summary_index: u32, delta: String },
    Content { content_index: u32, delta: String },
}

#[derive(Clone)]
struct ReasoningBlock {
    item_id: String,
    summary_index: u32,
    summary: String,
    content: String,
    segments: Vec<ReasoningSegment>,
}

#[derive(Clone)]
struct ToolBlock {
    item_id: String,
    provider_call_id: String,
    identity: ProviderToolIdentity,
    arguments: String,
}

#[derive(Clone)]
enum OpenBlock {
    Text(TextBlock),
    Reasoning(ReasoningBlock),
    Tool(ToolBlock),
}

/// Stateful converter tied to one successful Gateway open receipt.
pub struct ProviderStreamConverter {
    model_exchange_id: String,
    model_id: String,
    leak_gate: CredentialLeakGate,
    provider_response_id: Option<String>,
    blocks: BTreeMap<u32, OpenBlock>,
    used_indices: BTreeSet<u32>,
    tool_calls: BTreeMap<String, u32>,
    usage: Option<ProviderTokenUsage>,
    next_sequence: u64,
    terminal: bool,
}

impl ProviderStreamConverter {
    /// Binds conversion to the exact route and Credential fingerprint snapshot
    /// returned by a successful [`crate::ProviderGateway::open`] call.
    #[must_use]
    pub fn from_gateway_receipt(receipt: &ProviderGatewayOpenReceipt) -> Self {
        Self {
            model_exchange_id: receipt.model_exchange_id.0.clone(),
            model_id: receipt.route.model_id.clone(),
            leak_gate: receipt.stream_leak_gate(),
            provider_response_id: None,
            blocks: BTreeMap::new(),
            used_indices: BTreeSet::new(),
            tool_calls: BTreeMap::new(),
            usage: None,
            next_sequence: 1,
            terminal: false,
        }
    }

    /// Converts one Provider event into zero or more ordered canonical frames.
    /// Content deltas are intentionally coalesced until their block ends. This
    /// makes output independent of Provider chunk segmentation and lets the
    /// leak gate inspect complete content before any content frame is emitted.
    ///
    /// # Errors
    ///
    /// Rejects invalid lifecycle order, identity drift, oversized content,
    /// Credential material, or events after a terminal outcome.
    pub fn ingest(
        &mut self,
        event: ProviderStreamEvent,
    ) -> Result<Vec<CanonicalModelStreamFrame>, ProviderStreamConversionError> {
        if self.terminal {
            return Err(ProviderStreamConversionError::new(
                ProviderStreamConversionErrorKind::AlreadyTerminal,
                "Provider stream already reached a terminal outcome",
            ));
        }
        let result = self.ingest_inner(event);
        if result.is_err() {
            self.blocks.clear();
            self.terminal = true;
        }
        result
    }

    #[allow(clippy::too_many_lines)]
    fn ingest_inner(
        &mut self,
        event: ProviderStreamEvent,
    ) -> Result<Vec<CanonicalModelStreamFrame>, ProviderStreamConversionError> {
        match event {
            ProviderStreamEvent::ResponseStarted {
                provider_response_id,
            } => {
                if self.provider_response_id.is_some() || !self.blocks.is_empty() {
                    return Err(ProviderStreamConversionError::protocol());
                }
                validate_token(&provider_response_id, 200)?;
                self.leak_gate
                    .inspect_bytes(
                        CredentialOutputBoundary::Event,
                        provider_response_id.as_bytes(),
                    )
                    .map_err(|_| credential_leak())?;
                self.provider_response_id = Some(provider_response_id);
                self.emit(vec![
                    ModelPortMessage::Created,
                    ModelPortMessage::ServerModel {
                        model: self.model_id.clone(),
                    },
                ])
            }
            ProviderStreamEvent::TextStarted { index } => {
                self.require_started()?;
                self.reserve_index(index)?;
                let item_id = item_id("msg", &self.model_exchange_id, &index.to_string());
                self.blocks.insert(
                    index,
                    OpenBlock::Text(TextBlock {
                        item_id: item_id.clone(),
                        text: String::new(),
                    }),
                );
                self.emit(vec![ModelPortMessage::OutputItemAdded {
                    item: ModelOutputItem::assistant_message(item_id, String::new()),
                }])
            }
            ProviderStreamEvent::TextDelta { index, delta } => {
                validate_delta(&delta)?;
                let OpenBlock::Text(block) = self
                    .blocks
                    .get_mut(&index)
                    .ok_or_else(ProviderStreamConversionError::protocol)?
                else {
                    return Err(ProviderStreamConversionError::protocol());
                };
                append_checked(&self.leak_gate, &mut block.text, &delta)?;
                Ok(Vec::new())
            }
            ProviderStreamEvent::TextEnded { index } => {
                let OpenBlock::Text(block) = self
                    .blocks
                    .remove(&index)
                    .ok_or_else(ProviderStreamConversionError::protocol)?
                else {
                    return Err(ProviderStreamConversionError::protocol());
                };
                let mut messages = Vec::new();
                if !block.text.is_empty() {
                    messages.push(ModelPortMessage::OutputTextDelta {
                        delta: block.text.clone(),
                    });
                }
                messages.push(ModelPortMessage::OutputItemDone {
                    item: ModelOutputItem::assistant_message(block.item_id, block.text),
                });
                self.emit(messages)
            }
            ProviderStreamEvent::ReasoningStarted {
                index,
                summary_index,
            } => {
                self.require_started()?;
                self.reserve_index(index)?;
                let item_id = item_id("rs", &self.model_exchange_id, &index.to_string());
                self.blocks.insert(
                    index,
                    OpenBlock::Reasoning(ReasoningBlock {
                        item_id: item_id.clone(),
                        summary_index,
                        summary: String::new(),
                        content: String::new(),
                        segments: Vec::new(),
                    }),
                );
                self.emit(vec![
                    ModelPortMessage::OutputItemAdded {
                        item: ModelOutputItem::reasoning(item_id, String::new()),
                    },
                    ModelPortMessage::ReasoningSummaryPartAdded { summary_index },
                ])
            }
            ProviderStreamEvent::ReasoningSummaryDelta {
                index,
                summary_index,
                delta,
            } => {
                validate_delta(&delta)?;
                let OpenBlock::Reasoning(block) = self
                    .blocks
                    .get_mut(&index)
                    .ok_or_else(ProviderStreamConversionError::protocol)?
                else {
                    return Err(ProviderStreamConversionError::protocol());
                };
                if block.summary_index != summary_index {
                    return Err(ProviderStreamConversionError::protocol());
                }
                append_checked(&self.leak_gate, &mut block.summary, &delta)?;
                push_reasoning_segment(
                    &mut block.segments,
                    ReasoningSegment::Summary {
                        summary_index,
                        delta,
                    },
                );
                Ok(Vec::new())
            }
            ProviderStreamEvent::ReasoningContentDelta {
                index,
                content_index,
                delta,
            } => {
                validate_delta(&delta)?;
                let OpenBlock::Reasoning(block) = self
                    .blocks
                    .get_mut(&index)
                    .ok_or_else(ProviderStreamConversionError::protocol)?
                else {
                    return Err(ProviderStreamConversionError::protocol());
                };
                append_checked(&self.leak_gate, &mut block.content, &delta)?;
                push_reasoning_segment(
                    &mut block.segments,
                    ReasoningSegment::Content {
                        content_index,
                        delta,
                    },
                );
                Ok(Vec::new())
            }
            ProviderStreamEvent::ReasoningEnded { index } => {
                let OpenBlock::Reasoning(block) = self
                    .blocks
                    .remove(&index)
                    .ok_or_else(ProviderStreamConversionError::protocol)?
                else {
                    return Err(ProviderStreamConversionError::protocol());
                };
                let mut messages = block
                    .segments
                    .into_iter()
                    .map(|segment| match segment {
                        ReasoningSegment::Summary {
                            summary_index,
                            delta,
                        } => ModelPortMessage::ReasoningSummaryDelta {
                            delta,
                            summary_index,
                        },
                        ReasoningSegment::Content {
                            content_index,
                            delta,
                        } => ModelPortMessage::ReasoningContentDelta {
                            delta,
                            content_index,
                        },
                    })
                    .collect::<Vec<_>>();
                messages.push(ModelPortMessage::ReasoningSummaryDone {
                    item_id: block.item_id.clone(),
                    text: block.summary.clone(),
                    summary_index: block.summary_index,
                });
                messages.push(ModelPortMessage::OutputItemDone {
                    item: ModelOutputItem::reasoning(block.item_id, block.summary),
                });
                self.emit(messages)
            }
            ProviderStreamEvent::ToolCallStarted {
                index,
                provider_call_id,
                identity,
            } => {
                self.require_started()?;
                self.reserve_index(index)?;
                validate_token(&provider_call_id, 200)?;
                if self.tool_calls.contains_key(&provider_call_id) {
                    return Err(ProviderStreamConversionError::protocol());
                }
                self.leak_gate
                    .inspect_bytes(CredentialOutputBoundary::Event, provider_call_id.as_bytes())
                    .and_then(|()| inspect_tool_identity(&self.leak_gate, &identity))
                    .map_err(|_| credential_leak())?;
                let item_id = item_id("fc", &self.model_exchange_id, &provider_call_id);
                self.tool_calls.insert(provider_call_id.clone(), index);
                self.blocks.insert(
                    index,
                    OpenBlock::Tool(ToolBlock {
                        item_id: item_id.clone(),
                        provider_call_id: provider_call_id.clone(),
                        identity: identity.clone(),
                        arguments: String::new(),
                    }),
                );
                self.emit(vec![ModelPortMessage::OutputItemAdded {
                    item: ModelOutputItem::tool(identity, item_id, provider_call_id, String::new()),
                }])
            }
            ProviderStreamEvent::ToolCallArgumentsDelta {
                index,
                provider_call_id,
                delta,
            } => {
                validate_delta(&delta)?;
                let OpenBlock::Tool(block) = self
                    .blocks
                    .get_mut(&index)
                    .ok_or_else(ProviderStreamConversionError::protocol)?
                else {
                    return Err(ProviderStreamConversionError::protocol());
                };
                if block.provider_call_id != provider_call_id {
                    return Err(ProviderStreamConversionError::protocol());
                }
                append_checked(&self.leak_gate, &mut block.arguments, &delta)?;
                Ok(Vec::new())
            }
            ProviderStreamEvent::ToolCallEnded {
                index,
                provider_call_id,
            } => {
                let OpenBlock::Tool(block) = self
                    .blocks
                    .remove(&index)
                    .ok_or_else(ProviderStreamConversionError::protocol)?
                else {
                    return Err(ProviderStreamConversionError::protocol());
                };
                if block.provider_call_id != provider_call_id {
                    return Err(ProviderStreamConversionError::protocol());
                }
                let mut messages = Vec::new();
                if !block.arguments.is_empty() {
                    messages.push(ModelPortMessage::ToolCallInputDelta {
                        item_id: block.item_id.clone(),
                        call_id: Some(block.provider_call_id.clone()),
                        delta: block.arguments.clone(),
                    });
                }
                messages.push(ModelPortMessage::OutputItemDone {
                    item: ModelOutputItem::tool(
                        block.identity,
                        block.item_id,
                        block.provider_call_id,
                        block.arguments,
                    ),
                });
                self.emit(messages)
            }
            ProviderStreamEvent::Usage(usage) => {
                self.require_started()?;
                if self.usage.is_some() {
                    return Err(ProviderStreamConversionError::protocol());
                }
                validate_usage(usage)?;
                self.usage = Some(usage);
                Ok(Vec::new())
            }
            ProviderStreamEvent::Finished(reason) => {
                self.require_started()?;
                if !self.blocks.is_empty() {
                    return Err(ProviderStreamConversionError::protocol());
                }
                let message = match reason {
                    ProviderFinishReason::Stop if self.used_indices.is_empty() => {
                        ModelPortMessage::Error {
                            error: ModelFailure::new(
                                "EMPTY_RESPONSE",
                                "Provider completed without any response content",
                            ),
                        }
                    }
                    ProviderFinishReason::Stop | ProviderFinishReason::ToolCalls => {
                        ModelPortMessage::Completed {
                            response_id: self
                                .provider_response_id
                                .clone()
                                .ok_or_else(ProviderStreamConversionError::protocol)?,
                            token_usage: self.usage.map(ModelTokenUsage::from),
                            end_turn: Some(reason == ProviderFinishReason::Stop),
                        }
                    }
                    ProviderFinishReason::MaxTokens => ModelPortMessage::Error {
                        error: ModelFailure::new(
                            "MAX_TOKENS",
                            "Provider response reached its output-token limit",
                        ),
                    },
                };
                let frames = self.emit(vec![message])?;
                self.terminal = true;
                Ok(frames)
            }
            ProviderStreamEvent::Failed(failure) => {
                validate_failure(&self.leak_gate, &failure)?;
                let frames = self.emit(vec![ModelPortMessage::Error {
                    error: ModelFailure::from_provider(failure),
                }])?;
                self.blocks.clear();
                self.terminal = true;
                Ok(frames)
            }
            ProviderStreamEvent::Cancelled => {
                let frames = self.emit(vec![ModelPortMessage::Error {
                    error: ModelFailure::new("CANCELLED", "Model stream was cancelled"),
                }])?;
                self.blocks.clear();
                self.terminal = true;
                Ok(frames)
            }
            ProviderStreamEvent::Disconnected => {
                let frames = self.emit(vec![ModelPortMessage::Error {
                    error: ModelFailure::new(
                        "STREAM_CLOSED",
                        "Provider stream ended without a terminal event",
                    ),
                }])?;
                self.blocks.clear();
                self.terminal = true;
                Ok(frames)
            }
        }
    }

    fn require_started(&self) -> Result<(), ProviderStreamConversionError> {
        if self.provider_response_id.is_none() {
            return Err(ProviderStreamConversionError::protocol());
        }
        Ok(())
    }

    fn reserve_index(&mut self, index: u32) -> Result<(), ProviderStreamConversionError> {
        if self.blocks.len() >= MAX_OPEN_BLOCKS
            || self.used_indices.len() >= MAX_STREAM_BLOCKS
            || !self.used_indices.insert(index)
        {
            return Err(ProviderStreamConversionError::protocol());
        }
        Ok(())
    }

    fn emit(
        &mut self,
        messages: Vec<ModelPortMessage>,
    ) -> Result<Vec<CanonicalModelStreamFrame>, ProviderStreamConversionError> {
        let mut payloads = Vec::with_capacity(messages.len());
        for message in &messages {
            let payload = serde_json::to_vec(message)
                .map_err(|_| ProviderStreamConversionError::invalid())?;
            self.leak_gate
                .inspect_json_bytes(CredentialOutputBoundary::Event, &payload)
                .map_err(|_| credential_leak())?;
            payloads.push(
                String::from_utf8(payload).map_err(|_| ProviderStreamConversionError::invalid())?,
            );
        }
        let count = u64::try_from(payloads.len()).map_err(|_| sequence_overflow())?;
        let last_sequence = self
            .next_sequence
            .checked_add(count)
            .and_then(|value| value.checked_sub(1))
            .filter(|value| *value <= MAX_SAFE_INTEGER)
            .ok_or_else(sequence_overflow)?;
        let first_sequence = self.next_sequence;
        self.next_sequence = last_sequence.checked_add(1).ok_or_else(sequence_overflow)?;
        Ok(messages
            .into_iter()
            .zip(payloads)
            .zip(first_sequence..)
            .map(
                |((message, payload_json), sequence)| CanonicalModelStreamFrame {
                    sequence,
                    payload_json,
                    terminal: message.is_terminal(),
                },
            )
            .collect())
    }
}

fn append_checked(
    leak_gate: &CredentialLeakGate,
    current: &mut String,
    delta: &str,
) -> Result<(), ProviderStreamConversionError> {
    let next_length = current
        .len()
        .checked_add(delta.len())
        .filter(|length| *length <= MAX_CONTENT_BYTES)
        .ok_or_else(size_limit)?;
    current.push_str(delta);
    debug_assert_eq!(current.len(), next_length);
    leak_gate
        .inspect_bytes(CredentialOutputBoundary::Event, current.as_bytes())
        .map_err(|_| credential_leak())
}

fn push_reasoning_segment(segments: &mut Vec<ReasoningSegment>, segment: ReasoningSegment) {
    match (segments.last_mut(), segment) {
        (
            Some(ReasoningSegment::Summary {
                summary_index: previous_index,
                delta: previous,
            }),
            ReasoningSegment::Summary {
                summary_index,
                delta,
            },
        ) if *previous_index == summary_index => previous.push_str(&delta),
        (
            Some(ReasoningSegment::Content {
                content_index: previous_index,
                delta: previous,
            }),
            ReasoningSegment::Content {
                content_index,
                delta,
            },
        ) if *previous_index == content_index => previous.push_str(&delta),
        (_, segment) => segments.push(segment),
    }
}

fn validate_delta(value: &str) -> Result<(), ProviderStreamConversionError> {
    if value.is_empty() || value.len() > MAX_CONTENT_BYTES {
        return Err(ProviderStreamConversionError::invalid());
    }
    Ok(())
}

fn validate_token(value: &str, max_len: usize) -> Result<(), ProviderStreamConversionError> {
    if value.is_empty()
        || value.len() > max_len
        || value.trim() != value
        || value.chars().any(char::is_control)
    {
        return Err(ProviderStreamConversionError::invalid());
    }
    Ok(())
}

fn validate_tool_component(value: &str, max_len: usize) -> Result<(), ()> {
    if value.is_empty()
        || value.len() > max_len
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        return Err(());
    }
    Ok(())
}

fn inspect_tool_identity(
    leak_gate: &CredentialLeakGate,
    identity: &ProviderToolIdentity,
) -> Result<(), crate::CredentialLeakError> {
    leak_gate.inspect_bytes(CredentialOutputBoundary::Event, identity.name().as_bytes())?;
    if let Some(namespace) = identity.namespace() {
        leak_gate.inspect_bytes(CredentialOutputBoundary::Event, namespace.as_bytes())?;
    }
    Ok(())
}

fn validate_usage(usage: ProviderTokenUsage) -> Result<(), ProviderStreamConversionError> {
    let values = [
        usage.input_tokens,
        usage.cached_input_tokens,
        usage.cache_write_input_tokens,
        usage.output_tokens,
        usage.reasoning_output_tokens,
    ];
    if values.into_iter().any(|value| value > MAX_SAFE_INTEGER)
        || usage
            .input_tokens
            .checked_add(usage.output_tokens)
            .is_none_or(|total| total > MAX_SAFE_INTEGER)
    {
        return Err(ProviderStreamConversionError::invalid());
    }
    Ok(())
}

fn validate_failure(
    leak_gate: &CredentialLeakGate,
    failure: &ProviderStreamFailure,
) -> Result<(), ProviderStreamConversionError> {
    if failure
        .status
        .is_some_and(|status| !(100..=599).contains(&status))
        || failure
            .retry_after_millis
            .is_some_and(|delay| delay > 86_400_000)
    {
        return Err(ProviderStreamConversionError::invalid());
    }
    if let Some(provider_request_id) = &failure.provider_request_id {
        validate_token(provider_request_id, 200)?;
        leak_gate
            .inspect_bytes(
                CredentialOutputBoundary::Event,
                provider_request_id.as_bytes(),
            )
            .map_err(|_| credential_leak())?;
    }
    Ok(())
}

fn item_id(prefix: &str, model_exchange_id: &str, stable_key: &str) -> String {
    let digest = Sha256::digest(format!("{model_exchange_id}\0{prefix}\0{stable_key}"));
    let mut encoded = String::with_capacity(prefix.len() + 33);
    encoded.push_str(prefix);
    encoded.push('_');
    for byte in &digest[..16] {
        write!(encoded, "{byte:02x}").expect("writing to a String cannot fail");
    }
    encoded
}

fn credential_leak() -> ProviderStreamConversionError {
    ProviderStreamConversionError::new(
        ProviderStreamConversionErrorKind::CredentialLeak,
        "Provider stream output was rejected by the Credential leak gate",
    )
}

fn size_limit() -> ProviderStreamConversionError {
    ProviderStreamConversionError::new(
        ProviderStreamConversionErrorKind::SizeLimit,
        "Provider stream content exceeds the bounded size",
    )
}

fn sequence_overflow() -> ProviderStreamConversionError {
    ProviderStreamConversionError::new(
        ProviderStreamConversionErrorKind::SequenceOverflow,
        "Provider stream sequence is exhausted",
    )
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ModelPortMessage {
    Created,
    ServerModel {
        model: String,
    },
    OutputItemAdded {
        item: ModelOutputItem,
    },
    OutputItemDone {
        item: ModelOutputItem,
    },
    OutputTextDelta {
        delta: String,
    },
    ToolCallInputDelta {
        #[serde(rename = "itemId")]
        item_id: String,
        #[serde(rename = "callId")]
        call_id: Option<String>,
        delta: String,
    },
    ReasoningSummaryDelta {
        delta: String,
        #[serde(rename = "summaryIndex")]
        summary_index: u32,
    },
    ReasoningSummaryDone {
        #[serde(rename = "itemId")]
        item_id: String,
        text: String,
        #[serde(rename = "summaryIndex")]
        summary_index: u32,
    },
    ReasoningContentDelta {
        delta: String,
        #[serde(rename = "contentIndex")]
        content_index: u32,
    },
    ReasoningSummaryPartAdded {
        #[serde(rename = "summaryIndex")]
        summary_index: u32,
    },
    Completed {
        #[serde(rename = "responseId")]
        response_id: String,
        #[serde(rename = "tokenUsage", skip_serializing_if = "Option::is_none")]
        token_usage: Option<ModelTokenUsage>,
        #[serde(rename = "endTurn", skip_serializing_if = "Option::is_none")]
        end_turn: Option<bool>,
    },
    Error {
        error: ModelFailure,
    },
}

impl ModelPortMessage {
    const fn is_terminal(&self) -> bool {
        matches!(self, Self::Completed { .. } | Self::Error { .. })
    }
}

#[derive(Serialize)]
#[serde(tag = "type")]
enum ModelOutputItem {
    #[serde(rename = "message")]
    Message {
        id: String,
        role: &'static str,
        content: Vec<ModelOutputContent>,
        phase: &'static str,
    },
    #[serde(rename = "reasoning")]
    Reasoning {
        id: String,
        summary: Vec<ModelReasoningSummary>,
        encrypted_content: Option<String>,
    },
    #[serde(rename = "function_call")]
    FunctionCall {
        id: String,
        name: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        namespace: Option<String>,
        arguments: String,
        call_id: String,
    },
    #[serde(rename = "custom_tool_call")]
    CustomToolCall {
        id: String,
        call_id: String,
        name: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        namespace: Option<String>,
        input: String,
    },
}

impl ModelOutputItem {
    fn assistant_message(id: String, text: String) -> Self {
        Self::Message {
            id,
            role: "assistant",
            content: vec![ModelOutputContent {
                kind: "output_text",
                text,
            }],
            phase: "final_answer",
        }
    }

    fn reasoning(id: String, text: String) -> Self {
        Self::Reasoning {
            id,
            summary: vec![ModelReasoningSummary {
                kind: "summary_text",
                text,
            }],
            encrypted_content: None,
        }
    }

    fn tool(
        identity: ProviderToolIdentity,
        id: String,
        call_id: String,
        arguments: String,
    ) -> Self {
        match identity.kind {
            ProviderToolKind::Function => Self::FunctionCall {
                id,
                name: identity.name,
                namespace: identity.namespace,
                arguments,
                call_id,
            },
            ProviderToolKind::Custom => Self::CustomToolCall {
                id,
                call_id,
                name: identity.name,
                namespace: identity.namespace,
                input: arguments,
            },
        }
    }
}

#[derive(Serialize)]
struct ModelOutputContent {
    #[serde(rename = "type")]
    kind: &'static str,
    text: String,
}

#[derive(Serialize)]
struct ModelReasoningSummary {
    #[serde(rename = "type")]
    kind: &'static str,
    text: String,
}

#[derive(Serialize)]
#[allow(clippy::struct_field_names)] // Exact field names are the canonical ModelPort wire contract.
struct ModelTokenUsage {
    input_tokens: u64,
    cached_input_tokens: u64,
    cache_write_input_tokens: u64,
    output_tokens: u64,
    reasoning_output_tokens: u64,
    total_tokens: u64,
}

impl From<ProviderTokenUsage> for ModelTokenUsage {
    fn from(usage: ProviderTokenUsage) -> Self {
        Self {
            input_tokens: usage.input_tokens,
            cached_input_tokens: usage.cached_input_tokens,
            cache_write_input_tokens: usage.cache_write_input_tokens,
            output_tokens: usage.output_tokens,
            reasoning_output_tokens: usage.reasoning_output_tokens,
            total_tokens: usage.input_tokens + usage.output_tokens,
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ModelFailure {
    code: &'static str,
    message: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    status: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    provider_retry_after_millis: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    provider_request_id: Option<String>,
}

impl ModelFailure {
    const fn new(code: &'static str, message: &'static str) -> Self {
        Self {
            code,
            message,
            status: None,
            provider_retry_after_millis: None,
            provider_request_id: None,
        }
    }

    fn from_provider(failure: ProviderStreamFailure) -> Self {
        let (code, message) = match failure.kind {
            ProviderStreamFailureKind::Authentication => ("AUTH", "Provider authentication failed"),
            ProviderStreamFailureKind::InvalidRequest => {
                ("INVALID_REQUEST", "Provider rejected the request")
            }
            ProviderStreamFailureKind::RateLimit => {
                ("RATE_LIMIT", "Provider rate limit was reached")
            }
            ProviderStreamFailureKind::Quota => ("QUOTA", "Provider quota was exhausted"),
            ProviderStreamFailureKind::Timeout => ("TIMEOUT", "Provider request timed out"),
            ProviderStreamFailureKind::Transport => ("TRANSPORT", "Provider transport failed"),
            ProviderStreamFailureKind::Server => ("SERVER", "Provider server failed"),
            ProviderStreamFailureKind::ContextWindowExceeded => (
                "CONTEXT_WINDOW_EXCEEDED",
                "Provider context window was exceeded",
            ),
            ProviderStreamFailureKind::Unknown => {
                ("PROVIDER_STREAM_FAILED", "Provider stream failed")
            }
        };
        Self {
            code,
            message,
            status: failure.status,
            provider_retry_after_millis: failure.retry_after_millis,
            provider_request_id: failure.provider_request_id.map(|provider_request_id| {
                format!(
                    "sha256:{:x}",
                    Sha256::digest(provider_request_id.as_bytes())
                )
            }),
        }
    }
}
