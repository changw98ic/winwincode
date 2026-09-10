// SPDX-License-Identifier: Apache-2.0

//! Wire types for the outbound, device-initiated preview tunnel.

use serde::{Deserialize, Serialize};

/// Current preview-tunnel wire version.
pub const PREVIEW_TUNNEL_SCHEMA_VERSION: &str = "winwincode/preview-v1";
/// Largest HTTP body accepted in either direction.
pub const MAX_PREVIEW_BODY_BYTES: usize = 8 * 1024 * 1024;
/// Largest header count accepted in either direction.
pub const MAX_PREVIEW_HEADERS: usize = 64;

/// Stable identity of one locally authorized application service.
///
/// Local host names and ports never cross the tunnel. The Device Client maps
/// this identity to one exact loopback socket that its run supervisor owns.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreviewSourceDescriptor {
    #[serde(rename = "sourceId")]
    pub source_id: String,
    #[serde(rename = "workerSessionId")]
    pub worker_session_id: String,
    #[serde(rename = "repositoryBindingId")]
    pub repository_binding_id: String,
    pub mode: PreviewSourceMode,
    #[serde(rename = "candidateCommit", skip_serializing_if = "Option::is_none")]
    pub candidate_commit: Option<String>,
}

/// Whether a source follows a mutable development checkout or one frozen
/// candidate. The two modes are never interchangeable evidence.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PreviewSourceMode {
    Live,
    FrozenCandidate,
}

/// One safe HTTP header carried by the tunnel.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreviewHeader {
    pub name: String,
    pub value: String,
}

/// Device-to-Backend frame on the authenticated tunnel.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum DevicePreviewFrame {
    Register {
        #[serde(rename = "schemaVersion")]
        schema_version: String,
        sources: Vec<PreviewSourceDescriptor>,
    },
    HttpResponse {
        #[serde(rename = "requestId")]
        request_id: String,
        status: u16,
        headers: Vec<PreviewHeader>,
        #[serde(rename = "bodyBase64")]
        body_base64: String,
    },
}

/// Backend-to-device frame on the authenticated tunnel.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum ServerPreviewFrame {
    HttpRequest {
        #[serde(rename = "requestId")]
        request_id: String,
        #[serde(rename = "sourceId")]
        source_id: String,
        method: String,
        target: String,
        headers: Vec<PreviewHeader>,
        #[serde(rename = "bodyBase64")]
        body_base64: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preview_frames_never_carry_a_local_address() {
        let frame = DevicePreviewFrame::Register {
            schema_version: PREVIEW_TUNNEL_SCHEMA_VERSION.to_owned(),
            sources: vec![PreviewSourceDescriptor {
                source_id: "pvs_demo".to_owned(),
                worker_session_id: "ws_demo".to_owned(),
                repository_binding_id: "rbd_demo".to_owned(),
                mode: PreviewSourceMode::FrozenCandidate,
                candidate_commit: Some("a".repeat(40)),
            }],
        };
        let json = serde_json::to_string(&frame).expect("serializes");
        assert!(!json.contains("localhost"));
        assert!(!json.contains("127.0.0.1"));
        assert!(!json.contains("169.254.169.254"));
        assert!(!json.contains("port"));
        assert_eq!(
            serde_json::from_str::<DevicePreviewFrame>(&json).expect("parses"),
            frame
        );
    }
}
