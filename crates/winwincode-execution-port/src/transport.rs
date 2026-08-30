// SPDX-License-Identifier: Apache-2.0

//! Transport adapters for the canonical [`ExecutionPortMessage`] union.
//!
//! This module owns only the transport seam.  The [`ExecutionPortCore`]
//! implementation remains responsible for lease, deduplication, ordering,
//! and execution decisions.  The local and remote adapters therefore forward
//! exactly the same typed message to that core after validating the frame.

use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::generated::ExecutionPortMessage;

/// The direction encoded in an `ExecutionPort` frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FrameDirection {
    /// A message sent by the worker to the Control Plane.
    #[serde(rename = "worker-to-control-plane")]
    WorkerToControlPlane,
    /// A message sent by the Control Plane to the worker.
    #[serde(rename = "control-plane-to-worker")]
    ControlPlaneToWorker,
}

impl FrameDirection {
    /// Resolves the canonical direction for one generated message.
    ///
    /// The generated union is intentionally untagged.  Inspecting its
    /// canonical `kind` field keeps this module independent of message body
    /// details while still making the direction table exhaustive at runtime.
    ///
    /// # Errors
    ///
    /// Returns an error when a message cannot be serialized to an object with
    /// a known string `kind`.
    pub fn for_message(message: &ExecutionPortMessage) -> Result<Self, FrameError> {
        let value = serde_json::to_value(message)
            .map_err(|error| FrameError::Serialization(error.to_string()))?;
        let kind = value
            .get("kind")
            .and_then(Value::as_str)
            .ok_or_else(|| FrameError::UnknownMessageKind("missing kind".to_owned()))?;

        match kind {
            "worker.register"
            | "worker.capabilities"
            | "worker.heartbeat"
            | "job.dispatch_result"
            | "session.binding"
            | "runtime.event"
            | "artifact.open"
            | "artifact.chunk"
            | "model.open"
            | "model.ack"
            | "input.request"
            | "approval.request"
            | "action.enforcement_request"
            | "job.cancel_ack"
            | "job.outcome" => Ok(Self::WorkerToControlPlane),
            "worker.registration_result"
            | "worker.heartbeat_ack"
            | "job.dispatch"
            | "lease.renew"
            | "runtime.ack"
            | "runtime.replay_request"
            | "artifact.ack"
            | "model.chunk"
            | "input.response"
            | "approval.decision"
            | "action.enforcement_receipt"
            | "job.cancel"
            | "job.outcome_ack" => Ok(Self::ControlPlaneToWorker),
            other => Err(FrameError::UnknownMessageKind(other.to_owned())),
        }
    }
}

/// The endpoint at which an adapter is operating.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndpointSide {
    /// The Control Plane consumes worker-to-Control Plane messages.
    ControlPlane,
    /// The worker consumes Control Plane-to-worker messages.
    Worker,
}

impl EndpointSide {
    fn expected_direction(self) -> FrameDirection {
        match self {
            Self::ControlPlane => FrameDirection::WorkerToControlPlane,
            Self::Worker => FrameDirection::ControlPlaneToWorker,
        }
    }
}

/// A generated message paired with its validated transport direction.
#[derive(Debug, Clone, PartialEq)]
pub struct TypedFrame {
    direction: FrameDirection,
    message: ExecutionPortMessage,
}

impl TypedFrame {
    /// Creates a frame after checking the direction against the message kind.
    ///
    /// # Errors
    ///
    /// Returns [`FrameError::DirectionMismatch`] when the declared direction
    /// is not the direction assigned to the message kind.
    pub fn new(
        direction: FrameDirection,
        message: ExecutionPortMessage,
    ) -> Result<Self, FrameError> {
        let expected = FrameDirection::for_message(&message)?;
        if direction != expected {
            return Err(FrameError::DirectionMismatch {
                declared: direction,
                expected,
            });
        }
        Ok(Self { direction, message })
    }

    /// Returns the validated direction.
    #[must_use]
    pub const fn direction(&self) -> FrameDirection {
        self.direction
    }

    /// Returns the generated message carried by this frame.
    #[must_use]
    pub const fn message(&self) -> &ExecutionPortMessage {
        &self.message
    }
}

/// Errors raised while validating or decoding an `ExecutionPort` frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrameError {
    /// The JSON envelope or generated message is malformed.
    Malformed(String),
    /// The declared direction differs from the canonical message direction.
    DirectionMismatch {
        /// Direction declared by the frame.
        declared: FrameDirection,
        /// Direction required by the message kind.
        expected: FrameDirection,
    },
    /// A transport error frame was received instead of an executable message.
    ErrorFrame,
    /// The message kind is not part of the generated union's direction table.
    UnknownMessageKind(String),
    /// The generated message could not be converted to the wire form.
    Serialization(String),
}

impl fmt::Display for FrameError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Malformed(reason) => {
                write!(formatter, "malformed execution-port frame: {reason}")
            }
            Self::DirectionMismatch { declared, expected } => write!(
                formatter,
                "execution-port direction mismatch: declared {declared:?}, expected {expected:?}"
            ),
            Self::ErrorFrame => formatter.write_str("execution-port error frame"),
            Self::UnknownMessageKind(kind) => {
                write!(formatter, "unknown execution-port message kind: {kind}")
            }
            Self::Serialization(reason) => {
                write!(
                    formatter,
                    "execution-port frame serialization failed: {reason}"
                )
            }
        }
    }
}

impl std::error::Error for FrameError {}

/// Error returned by an adapter before or while invoking the shared core.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdapterError<CoreError> {
    /// The frame was rejected at the transport seam.
    Frame(FrameError),
    /// The shared business core rejected the generated message.
    Core(CoreError),
}

impl<CoreError: fmt::Display> fmt::Display for AdapterError<CoreError> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Frame(error) => error.fmt(formatter),
            Self::Core(error) => write!(formatter, "execution-port core rejected message: {error}"),
        }
    }
}

impl<CoreError: std::error::Error + 'static> std::error::Error for AdapterError<CoreError> {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Frame(error) => Some(error),
            Self::Core(error) => Some(error),
        }
    }
}

/// Business-neutral execution core shared by both adapters.
///
/// Implementations own all execution semantics.  In particular, this trait
/// is the only place that decides whether a message is accepted, duplicated,
/// conflicting, out of order, expired, stale, or requires reacquisition.
pub trait ExecutionPortCore {
    /// Result returned for an accepted message.
    type Output;
    /// Business error returned for a rejected message.
    type Error;

    /// Accepts one already-decoded generated message.
    ///
    /// # Errors
    ///
    /// Returns the core's business error when the message is rejected by the
    /// shared execution implementation.
    fn accept(&mut self, message: &ExecutionPortMessage) -> Result<Self::Output, Self::Error>;
}

/// Local same-process adapter for the shared execution core.
pub struct LocalWorkerAdapter<'core, Core: ExecutionPortCore + ?Sized> {
    core: &'core mut Core,
    side: EndpointSide,
}

impl<'core, Core: ExecutionPortCore + ?Sized> LocalWorkerAdapter<'core, Core> {
    /// Creates a local adapter for one endpoint side.
    #[must_use]
    pub fn new(core: &'core mut Core, side: EndpointSide) -> Self {
        Self { core, side }
    }

    /// Validates and forwards one typed frame to the shared core.
    ///
    /// # Errors
    ///
    /// Returns a frame error when the frame is for the other endpoint, or the
    /// core's error when the shared execution implementation rejects it.
    pub fn accept(
        &mut self,
        frame: &TypedFrame,
    ) -> Result<Core::Output, AdapterError<Core::Error>> {
        self.check_direction(frame.direction())?;
        self.core
            .accept(frame.message())
            .map_err(AdapterError::Core)
    }

    fn check_direction(&self, direction: FrameDirection) -> Result<(), AdapterError<Core::Error>> {
        let expected = self.side.expected_direction();
        if direction != expected {
            return Err(AdapterError::Frame(FrameError::DirectionMismatch {
                declared: direction,
                expected,
            }));
        }
        Ok(())
    }
}

/// Remote JSON adapter for the shared execution core.
pub struct RemoteTransportAdapter<'core, Core: ExecutionPortCore + ?Sized> {
    core: &'core mut Core,
    side: EndpointSide,
}

impl<'core, Core: ExecutionPortCore + ?Sized> RemoteTransportAdapter<'core, Core> {
    /// Creates a remote adapter for one endpoint side.
    #[must_use]
    pub fn new(core: &'core mut Core, side: EndpointSide) -> Self {
        Self { core, side }
    }

    /// Encodes a validated typed frame as the canonical JSON message envelope.
    ///
    /// The generated message remains the only source of message fields; this
    /// envelope adds only transport direction and frame type.
    ///
    /// # Errors
    ///
    /// Returns a serialization error if the generated message cannot be
    /// represented as JSON.
    pub fn encode(frame: &TypedFrame) -> Result<Vec<u8>, FrameError> {
        let wire = WireFrame::Message {
            direction: frame.direction,
            message: Box::new(frame.message.clone()),
        };
        serde_json::to_vec(&wire).map_err(|error| FrameError::Serialization(error.to_string()))
    }

    /// Decodes and validates one JSON frame without invoking a core.
    ///
    /// # Errors
    ///
    /// Returns a malformed-frame error for invalid JSON, unknown envelope
    /// fields, or an invalid generated message; returns [`FrameError::ErrorFrame`]
    /// for a well-formed transport error frame.
    pub fn decode(bytes: &[u8]) -> Result<TypedFrame, FrameError> {
        let value: Value = serde_json::from_slice(bytes)
            .map_err(|error| FrameError::Malformed(error.to_string()))?;
        validate_wire_fields(&value)?;
        let wire: WireFrame = serde_json::from_value(value)
            .map_err(|error| FrameError::Malformed(error.to_string()))?;

        match wire {
            WireFrame::Message { direction, message } => TypedFrame::new(direction, *message),
            WireFrame::Error { .. } => Err(FrameError::ErrorFrame),
        }
    }

    /// Decodes one remote frame and forwards its message to the shared core.
    ///
    /// # Errors
    ///
    /// Returns a frame error before the core is called when decoding, direction,
    /// or error-frame validation fails; otherwise returns the core's business
    /// error when the shared execution implementation rejects the message.
    pub fn accept(&mut self, bytes: &[u8]) -> Result<Core::Output, AdapterError<Core::Error>> {
        let frame = Self::decode(bytes).map_err(AdapterError::Frame)?;
        let expected = self.side.expected_direction();
        if frame.direction() != expected {
            return Err(AdapterError::Frame(FrameError::DirectionMismatch {
                declared: frame.direction(),
                expected,
            }));
        }
        self.core
            .accept(frame.message())
            .map_err(AdapterError::Core)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "frameType")]
enum WireFrame {
    #[serde(rename = "message")]
    Message {
        direction: FrameDirection,
        message: Box<ExecutionPortMessage>,
    },
    #[serde(rename = "error")]
    Error {
        direction: FrameDirection,
        error: WireError,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireError {
    code: String,
    message: String,
    retryable: bool,
}

fn validate_wire_fields(value: &Value) -> Result<(), FrameError> {
    let object = value
        .as_object()
        .ok_or_else(|| FrameError::Malformed("frame must be a JSON object".to_owned()))?;
    let frame_type = object
        .get("frameType")
        .and_then(Value::as_str)
        .ok_or_else(|| FrameError::Malformed("frameType must be a string".to_owned()))?;

    let allowed = match frame_type {
        "message" => ["frameType", "direction", "message"].as_slice(),
        "error" => ["frameType", "direction", "error"].as_slice(),
        other => {
            return Err(FrameError::Malformed(format!(
                "unknown frameType `{other}`"
            )));
        }
    };
    if let Some(field) = object
        .keys()
        .find(|field| !allowed.contains(&field.as_str()))
    {
        return Err(FrameError::Malformed(format!(
            "unknown frame field `{field}`"
        )));
    }
    Ok(())
}
