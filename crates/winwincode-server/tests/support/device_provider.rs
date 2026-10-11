// SPDX-License-Identifier: Apache-2.0

use sha2::{Digest, Sha256};
use winwincode_domain::{RequestId, Sha256Digest};
use winwincode_storage::{
    NewOutboxEvent, ProductStateStorage, ReceiptActorKey, ReceiptIdentity, ReceiptScopeKey,
    SqliteStorage, StateCommit,
};

pub fn route(node: &str) -> serde_json::Value {
    serde_json::json!({"providerId":"provider-main","modelId":"model-main","credentialReferenceId":format!("crd_0{}",&format!("{:X}",Sha256::digest(format!("{node}\nprovider-main")))[..25])})
}

pub fn stage(storage: &mut SqliteStorage, node: &str) {
    let stream = format!("device-provider:{node}");
    let previous = storage.load_state(&stream).expect("projection lookup");
    let snapshot = serde_json::json!({"clientNodeId":node,"revision":1,"encryptionPublicKey":"A".repeat(88),"providers":[{"config":{"providerId":"provider-main","displayName":"Test Provider","modelIds":["model-main"],"protocol":"anthropic_messages","endpoint":"https://example.com/v1/messages","enabled":true},"credentialConfigured":true}]});
    let bytes = serde_json::to_vec(&snapshot).expect("snapshot");
    if previous
        .as_ref()
        .is_some_and(|state| state.payload == bytes)
    {
        return;
    }
    let revision = previous.map_or(0, |state| state.revision);
    let identity = ReceiptIdentity::new(
        ReceiptActorKey::from_encoded(b"device-provider-test".to_vec()).expect("actor"),
        ReceiptScopeKey::from_encoded(stream.as_bytes().to_vec()).expect("scope"),
        RequestId(format!("provider-test-projection-{revision}")),
    )
    .expect("receipt");
    storage
        .commit(&StateCommit::new(
            identity,
            Sha256Digest(format!("sha256:{:x}", Sha256::digest(&bytes))),
            stream,
            revision,
            bytes,
            vec![NewOutboxEvent::internal(
                "provider-test-change",
                "device.provider.changed.v1",
                b"{}".to_vec(),
            )],
        ))
        .expect("device projection");
}

#[allow(dead_code)]
pub fn chat_scope() -> serde_json::Value {
    serde_json::json!({"kind":"repository","organizationId":"org_00000000000000000000000001","workspaceId":"wsp_00000000000000000000000001","projectId":"prj_00000000000000000000000001","repositoryId":"rep_00000000000000000000000001"})
}

#[allow(dead_code)]
pub fn stage_chat(directory: &std::path::Path, node: &str, user: &str, session: u64) {
    let mut storage = SqliteStorage::open(directory).expect("session storage");
    stage(&mut storage, node);
    let command = serde_json::from_value(serde_json::json!({"schemaVersion":"winwincode/v1","command":"session.create","requestId":format!("req_{session:026}"),"actor":{"kind":"user","id":user},"scope":chat_scope(),"expectedRevision":0,"payload":{"productSessionId":format!("psn_{session:026}"),"projectId":"prj_00000000000000000000000001","repositoryId":"rep_00000000000000000000000001","title":"Device chat","modelRoute":route(node)}})).expect("create command");
    let command = winwincode_control_plane::CreateProductSessionCommand::from_api(
        command,
        winwincode_domain::ControlPlaneEventId(format!("evt_{session:026}")),
        winwincode_domain::Instant("2026-09-04T12:00:03.000Z".into()),
    )
    .expect("session authority");
    winwincode_control_plane::ProductSessionService::new(&mut storage)
        .create(&command)
        .expect("session create");
}
