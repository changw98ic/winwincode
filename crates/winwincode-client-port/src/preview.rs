use serde::{Deserialize, Serialize};

pub const PREVIEW_TUNNEL_SCHEMA_VERSION: &str = "winwincode/preview-v1";
pub const MAX_PREVIEW_BODY_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_PREVIEW_HEADERS: usize = 64;

#[must_use]
pub fn is_canonical_git_commit(value: &str) -> bool {
    matches!(value.len(), 40 | 64)
        && value
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreviewSourceDescriptor {
    #[serde(rename = "sourceId")]
    pub source_id: String,
    #[serde(rename = "workRunId")]
    pub work_run_id: String,
    #[serde(rename = "repositoryBindingId")]
    pub repository_binding_id: String,
    pub mode: PreviewSourceMode,
    #[serde(rename = "candidateCommit", skip_serializing_if = "Option::is_none")]
    pub candidate_commit: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PreviewSourceMode {
    Live,
    FrozenCandidate,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreviewHeader {
    pub name: String,
    pub value: String,
}

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
                work_run_id: "wrn_demo".to_owned(),
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
        assert!(json.contains("\"workRunId\":\"wrn_demo\""));
        assert!(!json.contains("workerSessionId"));
        assert_eq!(
            serde_json::from_str::<DevicePreviewFrame>(&json).expect("parses"),
            frame
        );
    }
}
