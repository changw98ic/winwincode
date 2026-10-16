// SPDX-License-Identifier: Apache-2.0

use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use winwincode_domain::{ExecutionMessageId, Instant, WorkerId, WorkerInstanceId};

use crate::generated::ExecutionPortMessage;

pub const MAX_REMOTE_FRAME_BYTES: usize = 256 * 1024;
pub const MAX_REMOTE_REQUEST_BYTES: usize = 4 * MAX_REMOTE_FRAME_BYTES + 64 * 1024;
pub const MAX_REMOTE_RESPONSE_BYTES: usize = 2 * 1024 * 1024;
pub const MAX_REMOTE_ACKNOWLEDGEMENTS: usize = 128;
pub const MAX_REMOTE_DELIVERIES: usize = 128;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RemoteExchangeRequest {
    schema_version: String,
    worker_id: WorkerId,
    worker_instance_id: WorkerInstanceId,
    acknowledgements: Vec<ExecutionMessageId>,
    frame: Vec<u8>,
}

impl RemoteExchangeRequest {
    pub fn new(
        worker_id: WorkerId,
        worker_instance_id: WorkerInstanceId,
        acknowledgements: Vec<ExecutionMessageId>,
        frame: Vec<u8>,
    ) -> Result<Self, FrameError> {
        if frame.is_empty() {
            return Err(FrameError::Malformed("remote frame is empty".into()));
        }
        if acknowledgements.len() > MAX_REMOTE_ACKNOWLEDGEMENTS
            || frame.len() > MAX_REMOTE_FRAME_BYTES
        {
            return Err(FrameError::TooLarge);
        }
        let typed = RemoteTransportAdapter::<NoopCore>::decode(&frame)?;
        if typed.direction() != FrameDirection::WorkerToControlPlane {
            return Err(FrameError::DirectionMismatch {
                declared: typed.direction(),
                expected: FrameDirection::WorkerToControlPlane,
            });
        }
        let request = Self {
            schema_version: "execution-port.remote-exchange.v1".to_owned(),
            worker_id,
            worker_instance_id,
            acknowledgements,
            frame,
        };
        request.encode()?;
        Ok(request)
    }

    #[must_use]
    pub fn with_acceptance_receipt(mut self) -> Self {
        "execution-port.remote-exchange.v2".clone_into(&mut self.schema_version);
        self
    }

    #[must_use]
    pub fn supports_acceptance_receipt(&self) -> bool {
        self.schema_version == "execution-port.remote-exchange.v2"
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, FrameError> {
        if bytes.is_empty() {
            return Err(FrameError::Malformed("remote request is empty".into()));
        }
        if bytes.len() > MAX_REMOTE_REQUEST_BYTES {
            return Err(FrameError::TooLarge);
        }
        let request: Self = serde_json::from_slice(bytes)
            .map_err(|error| FrameError::Malformed(error.to_string()))?;
        if !matches!(
            request.schema_version.as_str(),
            "execution-port.remote-exchange.v1" | "execution-port.remote-exchange.v2"
        ) {
            return Err(FrameError::Malformed(
                "unsupported remote exchange schema".to_owned(),
            ));
        }
        let version = request.schema_version.clone();
        let mut checked = Self::new(
            request.worker_id,
            request.worker_instance_id,
            request.acknowledgements,
            request.frame,
        )?;
        checked.schema_version = version;
        checked.encode()?;
        Ok(checked)
    }

    pub fn encode(&self) -> Result<Vec<u8>, FrameError> {
        bounded_encoding(self, MAX_REMOTE_REQUEST_BYTES)
    }

    #[must_use]
    pub const fn worker_id(&self) -> &WorkerId {
        &self.worker_id
    }

    #[must_use]
    pub const fn worker_instance_id(&self) -> &WorkerInstanceId {
        &self.worker_instance_id
    }

    #[must_use]
    pub fn acknowledgements(&self) -> &[ExecutionMessageId] {
        &self.acknowledgements
    }

    #[must_use]
    pub fn frame(&self) -> &[u8] {
        &self.frame
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RemoteExchangeDelivery {
    pub delivery_id: ExecutionMessageId,
    pub frame: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RemoteExchangeResponse {
    schema_version: String,
    server_time: Instant,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    frame_accepted: Option<bool>,
    deliveries: Vec<RemoteExchangeDelivery>,
}

impl RemoteExchangeResponse {
    pub fn new(deliveries: Vec<RemoteExchangeDelivery>) -> Result<Self, FrameError> {
        let format = time::format_description::parse(
            "[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:3]Z",
        )
        .map_err(|error| FrameError::Malformed(error.to_string()))?;
        let now = time::OffsetDateTime::now_utc()
            .format(&format)
            .map_err(|error| FrameError::Malformed(error.to_string()))?;
        Self::new_at(deliveries, Instant(now))
    }

    pub fn new_at(
        deliveries: Vec<RemoteExchangeDelivery>,
        server_time: Instant,
    ) -> Result<Self, FrameError> {
        time::OffsetDateTime::parse(
            &server_time.0,
            &time::format_description::well_known::Rfc3339,
        )
        .map_err(|error| FrameError::Malformed(error.to_string()))?;
        if server_time.0.len() != 24
            || !server_time.0.ends_with('Z')
            || server_time.0.as_bytes()[10] != b'T'
            || server_time.0.as_bytes()[19] != b'.'
        {
            return Err(FrameError::Malformed(
                "serverTime must use UTC millisecond precision".into(),
            ));
        }
        if deliveries.len() > MAX_REMOTE_DELIVERIES {
            return Err(FrameError::TooLarge);
        }
        for delivery in &deliveries {
            if delivery.frame.is_empty() {
                return Err(FrameError::Malformed("remote delivery is empty".into()));
            }
            if delivery.frame.len() > MAX_REMOTE_FRAME_BYTES {
                return Err(FrameError::TooLarge);
            }
            let typed = RemoteTransportAdapter::<NoopCore>::decode(&delivery.frame)?;
            if typed.direction() != FrameDirection::ControlPlaneToWorker {
                return Err(FrameError::DirectionMismatch {
                    declared: typed.direction(),
                    expected: FrameDirection::ControlPlaneToWorker,
                });
            }
            if execution_message_id(typed.message())? != delivery.delivery_id {
                return Err(FrameError::Malformed(
                    "remote delivery identity does not match its frame".to_owned(),
                ));
            }
        }
        let response = Self {
            schema_version: "execution-port.remote-exchange.v1".to_owned(),
            server_time,
            frame_accepted: None,
            deliveries,
        };
        response.encode()?;
        Ok(response)
    }

    pub fn with_acceptance(
        deliveries: Vec<RemoteExchangeDelivery>,
        accepted: bool,
    ) -> Result<Self, FrameError> {
        let mut response = Self::new(deliveries)?;
        "execution-port.remote-exchange.v2".clone_into(&mut response.schema_version);
        response.frame_accepted = Some(accepted);
        response.encode()?;
        Ok(response)
    }

    pub fn with_acceptance_at(
        deliveries: Vec<RemoteExchangeDelivery>,
        accepted: bool,
        server_time: Instant,
    ) -> Result<Self, FrameError> {
        let mut response = Self::new_at(deliveries, server_time)?;
        "execution-port.remote-exchange.v2".clone_into(&mut response.schema_version);
        response.frame_accepted = Some(accepted);
        response.encode()?;
        Ok(response)
    }

    #[must_use]
    pub const fn server_time(&self) -> &Instant {
        &self.server_time
    }

    #[must_use]
    pub fn frame_accepted(&self) -> bool {
        self.frame_accepted.unwrap_or(true)
    }

    #[must_use]
    pub const fn has_acceptance_receipt(&self) -> bool {
        self.frame_accepted.is_some()
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, FrameError> {
        if bytes.is_empty() {
            return Err(FrameError::Malformed("remote response is empty".into()));
        }
        if bytes.len() > MAX_REMOTE_RESPONSE_BYTES {
            return Err(FrameError::TooLarge);
        }
        let response: Self = serde_json::from_slice(bytes)
            .map_err(|error| FrameError::Malformed(error.to_string()))?;
        if !matches!(
            (response.schema_version.as_str(), response.frame_accepted),
            ("execution-port.remote-exchange.v1", None)
                | ("execution-port.remote-exchange.v2", Some(_))
        ) {
            return Err(FrameError::Malformed(
                "unsupported remote exchange schema".to_owned(),
            ));
        }
        let mut checked = Self::new_at(response.deliveries, response.server_time)?;
        checked.schema_version = response.schema_version;
        checked.frame_accepted = response.frame_accepted;
        checked.encode()?;
        Ok(checked)
    }

    pub fn encode(&self) -> Result<Vec<u8>, FrameError> {
        bounded_encoding(self, MAX_REMOTE_RESPONSE_BYTES)
    }

    #[must_use]
    pub fn deliveries(&self) -> &[RemoteExchangeDelivery] {
        &self.deliveries
    }
}

fn bounded_encoding(value: &impl Serialize, limit: usize) -> Result<Vec<u8>, FrameError> {
    let bytes =
        serde_json::to_vec(value).map_err(|error| FrameError::Serialization(error.to_string()))?;
    if bytes.len() > limit {
        return Err(FrameError::TooLarge);
    }
    Ok(bytes)
}

pub fn execution_message_id(
    message: &ExecutionPortMessage,
) -> Result<ExecutionMessageId, FrameError> {
    let value = serde_json::to_value(message)
        .map_err(|error| FrameError::Serialization(error.to_string()))?;
    value
        .get("messageId")
        .cloned()
        .ok_or_else(|| FrameError::Malformed("messageId is required".to_owned()))
        .and_then(|value| {
            serde_json::from_value(value).map_err(|error| FrameError::Malformed(error.to_string()))
        })
}

struct NoopCore;

impl ExecutionPortCore for NoopCore {
    type Output = ();
    type Error = std::convert::Infallible;

    fn accept(&mut self, _message: &ExecutionPortMessage) -> Result<Self::Output, Self::Error> {
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FrameDirection {
    #[serde(rename = "worker-to-control-plane")]
    WorkerToControlPlane,
    #[serde(rename = "control-plane-to-worker")]
    ControlPlaneToWorker,
}

impl FrameDirection {
    pub fn for_message(message: &ExecutionPortMessage) -> Result<Self, FrameError> {
        let value = serde_json::to_value(message)
            .map_err(|error| FrameError::Serialization(error.to_string()))?;
        let kind = value
            .get("kind")
            .and_then(Value::as_str)
            .ok_or_else(|| FrameError::UnknownMessageKind("missing kind".to_owned()))?;

        match kind {
            "worker.register"
            | "worker.heartbeat"
            | "job.dispatch_result"
            | "session.binding"
            | "runtime.event"
            | "artifact.open"
            | "artifact.chunk"
            | "model.open"
            | "model.chunk"
            | "input.request"
            | "approval.request"
            | "action.enforcement_request"
            | "job.cancel_ack"
            | "snapshot.freeze_receipt"
            | "job.outcome" => Ok(Self::WorkerToControlPlane),
            "worker.registration_result"
            | "worker.heartbeat_ack"
            | "job.dispatch"
            | "snapshot.freeze_request"
            | "snapshot.verify"
            | "lease.renew"
            | "runtime.ack"
            | "runtime.replay_request"
            | "artifact.ack"
            | "model.ack"
            | "input.response"
            | "approval.decision"
            | "action.enforcement_receipt"
            | "job.cancel"
            | "job.outcome_ack" => Ok(Self::ControlPlaneToWorker),
            other => Err(FrameError::UnknownMessageKind(other.to_owned())),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndpointSide {
    ControlPlane,
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

#[derive(Debug, Clone, PartialEq)]
pub struct TypedFrame {
    direction: FrameDirection,
    message: ExecutionPortMessage,
}

impl TypedFrame {
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

    #[must_use]
    pub const fn direction(&self) -> FrameDirection {
        self.direction
    }

    #[must_use]
    pub const fn message(&self) -> &ExecutionPortMessage {
        &self.message
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrameError {
    TooLarge,
    Malformed(String),
    DirectionMismatch {
        declared: FrameDirection,
        expected: FrameDirection,
    },
    ErrorFrame,
    UnknownMessageKind(String),
    Serialization(String),
}

impl fmt::Display for FrameError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLarge => formatter.write_str("execution-port message exceeds its byte budget"),
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdapterError<CoreError> {
    Frame(FrameError),
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

pub trait ExecutionPortCore {
    type Output;
    type Error;

    fn accept(&mut self, message: &ExecutionPortMessage) -> Result<Self::Output, Self::Error>;
}

pub struct LocalWorkerAdapter<'core, Core: ExecutionPortCore + ?Sized> {
    core: &'core mut Core,
    side: EndpointSide,
}

impl<'core, Core: ExecutionPortCore + ?Sized> LocalWorkerAdapter<'core, Core> {
    #[must_use]
    pub fn new(core: &'core mut Core, side: EndpointSide) -> Self {
        Self { core, side }
    }

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

pub struct RemoteTransportAdapter<'core, Core: ExecutionPortCore + ?Sized> {
    core: &'core mut Core,
    side: EndpointSide,
}

impl<'core, Core: ExecutionPortCore + ?Sized> RemoteTransportAdapter<'core, Core> {
    #[must_use]
    pub fn new(core: &'core mut Core, side: EndpointSide) -> Self {
        Self { core, side }
    }

    pub fn encode(frame: &TypedFrame) -> Result<Vec<u8>, FrameError> {
        let wire = WireFrame::Message {
            direction: frame.direction,
            message: Box::new(frame.message.clone()),
        };
        serde_json::to_vec(&wire).map_err(|error| FrameError::Serialization(error.to_string()))
    }

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
