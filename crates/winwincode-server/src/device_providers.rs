// SPDX-License-Identifier: Apache-2.0

//! Encrypted Web → Device commands and public Device → Web receipts.

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::path::Path;
use winwincode_api::generated::{DeviceConfigurationEnvelope, DeviceProviderReport};
use winwincode_client_port::messages::{ServerToClientEnvelope, ServerToClientMessage};
use winwincode_control_plane::{AccessGrantService, ClientRegistryService};
use winwincode_domain::{Instant, RequestId, Sha256Digest};
use winwincode_storage::{
    ClientDownlinkAppend, ClientNodeRecord, ClientPresenceState, NewOutboxEvent,
    ProductStateStorage, ReceiptActorKey, ReceiptIdentity, ReceiptScopeKey, SqliteStorage,
    StateCommit,
};

#[derive(Clone, Copy)]
pub(crate) enum ConfigurationKind {
    Provider,
    Extension,
    Repository,
}
impl ConfigurationKind {
    const fn namespace(self) -> &'static str {
        match self {
            Self::Provider => "device-provider",
            Self::Extension => "device-extension",
            Self::Repository => "device-repository",
        }
    }
    fn message(
        self,
        payload: Box<winwincode_client_port::messages::ServerConfigurationApplyPayload>,
    ) -> ServerToClientMessage {
        match self {
            Self::Provider => ServerToClientMessage::ProviderApply(payload),
            Self::Extension => ServerToClientMessage::ExtensionApply(payload),
            Self::Repository => ServerToClientMessage::RepositoryRegister(payload),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum ProviderRelayError {
    Invalid,
    Denied,
    Offline,
    Conflict,
    Unavailable,
}

impl From<winwincode_storage::StorageError> for ProviderRelayError {
    fn from(_: winwincode_storage::StorageError) -> Self {
        Self::Unavailable
    }
}
impl From<serde_json::Error> for ProviderRelayError {
    fn from(_: serde_json::Error) -> Self {
        Self::Invalid
    }
}

fn node_for_user(
    storage: &mut SqliteStorage,
    user: &str,
    client: &str,
    now: &Instant,
) -> Result<ClientNodeRecord, ProviderRelayError> {
    let node = ClientRegistryService::new(storage)
        .snapshot_by_public_client_id(client)
        .map_err(|_| ProviderRelayError::Unavailable)?
        .ok_or(ProviderRelayError::Denied)?;
    let grant = AccessGrantService::new(storage)
        .active_grant(&node.client_node_id, user)
        .map_err(|_| ProviderRelayError::Unavailable)?
        .ok_or(ProviderRelayError::Denied)?;
    if !grant.permissions.can_manage()
        || grant.expires_at.is_some_and(|expiry| expiry.0 <= now.0)
        || node.presence_state == ClientPresenceState::Revoked
    {
        return Err(ProviderRelayError::Denied);
    }
    Ok(node)
}

fn online(node: &ClientNodeRecord, now: &Instant) -> bool {
    let Some(cutoff) = crate::client_occupancy::offset_instant(now, -45_000) else {
        return false;
    };
    node.presence_state == ClientPresenceState::Online
        && node
            .last_heartbeat_at
            .as_ref()
            .is_some_and(|stamp| stamp.0 >= cutoff.0)
}

pub(crate) fn read(
    directory: &Path,
    kind: ConfigurationKind,
    user: &str,
    client: &str,
    request_id: Option<&str>,
    now: &Instant,
) -> Result<Value, ProviderRelayError> {
    let namespace = kind.namespace();
    let mut storage = SqliteStorage::open(directory)?;
    let node = node_for_user(&mut storage, user, client, now)?;
    let snapshot = storage
        .load_state(&format!("{namespace}:{}", node.client_node_id))?
        .map(|state| serde_json::from_slice::<Value>(&state.payload))
        .transpose()?;
    let receipt = match request_id {
        Some(id) if valid_request_id(id) => storage
            .load_state(&format!("{namespace}-receipt:{}:{id}", node.client_node_id))?
            .map(|state| serde_json::from_slice::<Value>(&state.payload))
            .transpose()?,
        Some(_) => return Err(ProviderRelayError::Invalid),
        None => None,
    };
    Ok(
        json!({"schemaVersion":"winwincode/v1", "online": online(&node, now), "snapshot":snapshot, "receipt":receipt}),
    )
}

pub(crate) fn apply(
    directory: &Path,
    kind: ConfigurationKind,
    user: &str,
    client: &str,
    value: Value,
    now: &Instant,
) -> Result<Value, ProviderRelayError> {
    let envelope: DeviceConfigurationEnvelope = serde_json::from_value(value)?;
    if !valid_request_id(&envelope.request_id)
        || envelope.ciphertext.len() > 65_536
        || envelope.ciphertext.is_empty()
        || envelope.public_key.len() > 128
        || envelope.nonce.len() > 24
        || !(0..=9_007_199_254_740_991).contains(&envelope.expected_revision)
    {
        return Err(ProviderRelayError::Invalid);
    }
    let namespace = kind.namespace();
    let mut storage = SqliteStorage::open(directory)?;
    let node = node_for_user(&mut storage, user, client, now)?;
    if node.client_node_id != envelope.client_node_id {
        return Err(ProviderRelayError::Denied);
    }
    if !online(&node, now) {
        return Err(ProviderRelayError::Offline);
    }
    let stream = format!(
        "{namespace}-command:{}:{}",
        node.client_node_id, envelope.request_id
    );
    let payload = serde_json::to_vec(&envelope)?;
    if let Some(previous) = storage.load_state(&stream)? {
        if previous.payload != payload {
            return Err(ProviderRelayError::Conflict);
        }
    } else {
        write_state(&mut storage, &stream, &payload)?;
    }
    // A lost HTTP response may enqueue the same encrypted command again. Device receipts are
    // idempotent; repeating delivery also recovers a crash between command persistence and append.
    let cursors = ClientRegistryService::new(&mut storage)
        .exchange_cursors(&node.client_node_id)
        .map_err(|_| ProviderRelayError::Unavailable)?
        .ok_or(ProviderRelayError::Unavailable)?;
    let mut outbox = storage
        .client_downlink_outbox()
        .map_err(|_| ProviderRelayError::Unavailable)?;
    let sequence = outbox
        .high_water(&node.client_node_id)
        .map_err(|_| ProviderRelayError::Unavailable)?
        .max(cursors.server_to_client_ack_sequence)
        .checked_add(1)
        .ok_or(ProviderRelayError::Unavailable)?;
    let mut random = [0u8; 16];
    getrandom::fill(&mut random).map_err(|_| ProviderRelayError::Unavailable)?;
    let frame = ServerToClientEnvelope {
        schema_version: "winwincode/v1".to_owned(),
        message_id: format!("cmsg_{}", crate::runtime::crockford_26(&random)),
        client_node_id: node.client_node_id.clone(),
        client_instance_id: node
            .current_instance_id
            .ok_or(ProviderRelayError::Offline)?,
        sequence,
        occurred_at: now.0.clone(),
        message: kind.message(Box::new(
            winwincode_client_port::messages::ServerConfigurationApplyPayload {
                command: winwincode_client_port::messages::CommandContext {
                    expected_revision: u64::try_from(envelope.expected_revision)
                        .map_err(|_| ProviderRelayError::Invalid)?,
                    idempotency_key: envelope.request_id.clone(),
                },
                encrypted: envelope.clone(),
            },
        )),
    };
    outbox
        .append(
            &ClientDownlinkAppend::try_new(
                node.client_node_id,
                frame.message_id.clone(),
                sequence,
                serde_json::to_string(&frame)?,
            )
            .map_err(|_| ProviderRelayError::Unavailable)?,
            now,
        )
        .map_err(|_| ProviderRelayError::Unavailable)?;
    Ok(
        json!({"schemaVersion":"winwincode/v1", "requestId":envelope.request_id, "status":"waiting"}),
    )
}

pub(crate) fn observe(
    storage: &mut SqliteStorage,
    node_id: &str,
    report: &DeviceProviderReport,
) -> Result<(), ProviderRelayError> {
    if report.snapshot.client_node_id != node_id
        || report.snapshot.providers.len() > 100
        || !(0..=9_007_199_254_740_991).contains(&report.snapshot.revision)
        || report.snapshot.encryption_public_key.len() != 88
        || report.snapshot.providers.iter().any(|provider| {
            !winwincode_control_plane::valid_device_provider_config(&provider.config)
        })
        || report
            .snapshot
            .providers
            .iter()
            .map(|provider| &provider.config.provider_id)
            .collect::<std::collections::BTreeSet<_>>()
            .len()
            != report.snapshot.providers.len()
        || report.receipt.as_ref().is_some_and(|receipt| {
            receipt.revision < 0 || receipt.revision > report.snapshot.revision
        })
    {
        return Err(ProviderRelayError::Invalid);
    }
    observe_configuration(
        storage,
        ConfigurationKind::Provider,
        node_id,
        &serde_json::to_value(&report.snapshot)?,
        report
            .receipt
            .as_ref()
            .map(serde_json::to_value)
            .transpose()?
            .as_ref(),
    )
}

pub(crate) fn observe_extensions(
    storage: &mut SqliteStorage,
    node_id: &str,
    report: &winwincode_api::generated::DeviceExtensionReport,
) -> Result<(), ProviderRelayError> {
    let snapshot = &report.snapshot;
    let valid_id = |id: &str| {
        !id.is_empty()
            && id.len() <= 64
            && id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
    };
    let valid_digest = |value: &str| {
        value
            .strip_prefix("sha256:")
            .is_some_and(|hex| hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit()))
    };
    if snapshot.client_node_id != node_id
        || snapshot.skills.len() > 100
        || snapshot.mcp_servers.len() > 100
        || snapshot.encryption_public_key.len() != 88
        || !(0..=9_007_199_254_740_991).contains(&snapshot.revision)
        || snapshot.skills.iter().any(|skill| {
            !valid_id(&skill.id)
                || skill.name.is_empty()
                || skill.name.len() > 64
                || skill.description.len() > 1024
                || skill.source.len() > 2048
                || !(1..=256).contains(&skill.file_count)
                || !valid_digest(&skill.digest)
        })
        || snapshot.mcp_servers.iter().any(|server| {
            !valid_id(&server.id)
                || !valid_digest(&server.digest)
                || server.tool_names.len() > 128
                || server.tool_names.iter().any(|tool| {
                    tool.is_empty()
                        || tool.len() > 128
                        || !tool
                            .bytes()
                            .all(|b| b.is_ascii_alphanumeric() || b"_-.".contains(&b))
                })
                || server
                    .tool_names
                    .iter()
                    .collect::<std::collections::BTreeSet<_>>()
                    .len()
                    != server.tool_names.len()
                || (server.connection_status
                    != winwincode_api::generated::DeviceExtensionMcpConnectionStatus::Ready
                    && !server.tool_names.is_empty())
        })
        || snapshot
            .skills
            .iter()
            .map(|s| &s.id)
            .collect::<std::collections::BTreeSet<_>>()
            .len()
            != snapshot.skills.len()
        || snapshot
            .mcp_servers
            .iter()
            .map(|s| &s.id)
            .collect::<std::collections::BTreeSet<_>>()
            .len()
            != snapshot.mcp_servers.len()
        || report
            .receipt
            .as_ref()
            .is_some_and(|receipt| receipt.revision < 0 || receipt.revision > snapshot.revision)
    {
        return Err(ProviderRelayError::Invalid);
    }
    observe_configuration(
        storage,
        ConfigurationKind::Extension,
        node_id,
        &serde_json::to_value(snapshot)?,
        report
            .receipt
            .as_ref()
            .map(serde_json::to_value)
            .transpose()?
            .as_ref(),
    )
}

fn observe_configuration(
    storage: &mut SqliteStorage,
    kind: ConfigurationKind,
    node_id: &str,
    snapshot: &Value,
    receipt: Option<&Value>,
) -> Result<(), ProviderRelayError> {
    let namespace = kind.namespace();
    let stream = format!("{namespace}:{node_id}");
    let revision = snapshot["revision"]
        .as_i64()
        .ok_or(ProviderRelayError::Invalid)?;
    let previous = storage
        .load_state(&stream)?
        .map(|state| serde_json::from_slice::<Value>(&state.payload))
        .transpose()?;
    if previous.as_ref().is_some_and(|previous| {
        previous["revision"].as_i64() == Some(revision) && previous != snapshot
    }) {
        return Err(ProviderRelayError::Conflict);
    }
    if previous.as_ref().is_none_or(|previous| {
        previous["revision"]
            .as_i64()
            .is_some_and(|old| old < revision)
    }) {
        write_state(storage, &stream, &serde_json::to_vec(snapshot)?)?;
    }
    if let Some(receipt) = receipt {
        let id = receipt["requestId"]
            .as_str()
            .filter(|id| valid_request_id(id))
            .ok_or(ProviderRelayError::Invalid)?;
        let stream = format!("{namespace}-receipt:{node_id}:{id}");
        let payload = serde_json::to_vec(receipt)?;
        if let Some(previous) = storage.load_state(&stream)? {
            if previous.payload != payload {
                return Err(ProviderRelayError::Conflict);
            }
        } else {
            write_state(storage, &stream, &payload)?;
        }
    }
    Ok(())
}

pub(crate) fn observe_repository_registration(
    storage: &mut SqliteStorage,
    node_id: &str,
    receipt: &winwincode_api::generated::DeviceRepositoryRegistrationReceipt,
) -> Result<(), ProviderRelayError> {
    if (receipt.outcome == "registered") != receipt.repository_binding_id.is_some() {
        return Err(ProviderRelayError::Invalid);
    }
    if !valid_request_id(&receipt.request_id) {
        return Err(ProviderRelayError::Invalid);
    }
    let namespace = ConfigurationKind::Repository.namespace();
    if storage
        .load_state(&format!(
            "{namespace}-command:{node_id}:{}",
            receipt.request_id
        ))?
        .is_none()
    {
        return Err(ProviderRelayError::Invalid);
    }
    let stream = format!("{namespace}-receipt:{node_id}:{}", receipt.request_id);
    let payload = serde_json::to_vec(receipt)?;
    if let Some(previous) = storage.load_state(&stream)? {
        if previous.payload != payload {
            return Err(ProviderRelayError::Conflict);
        }
    } else {
        write_state(storage, &stream, &payload)?;
    }
    Ok(())
}

fn write_state(
    storage: &mut SqliteStorage,
    stream: &str,
    payload: &[u8],
) -> Result<(), ProviderRelayError> {
    let previous = storage.load_state(stream)?;
    if previous
        .as_ref()
        .is_some_and(|state| state.payload == payload)
    {
        return Ok(());
    }
    let revision = previous.map_or(0, |state| state.revision);
    let digest = format!("{:x}", Sha256::digest(payload));
    let identity = ReceiptIdentity::new(
        ReceiptActorKey::from_encoded(b"device-provider".to_vec())?,
        ReceiptScopeKey::from_encoded(stream.as_bytes().to_vec())?,
        RequestId(format!("provider-{revision}-{digest}")),
    )?;
    storage.commit(&StateCommit::new(
        identity,
        Sha256Digest(format!("sha256:{digest}")),
        stream,
        revision,
        payload,
        vec![NewOutboxEvent::internal(
            format!(
                "device-provider-{:x}-{digest}-{revision}",
                Sha256::digest(stream.as_bytes())
            ),
            "device.provider.changed.v1",
            br#"{"schemaVersion":"winwincode/v1"}"#.to_vec(),
        )],
    ))?;
    Ok(())
}

fn valid_request_id(value: &str) -> bool {
    (8..=200).contains(&value.len())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
}

fn route_reference(node: &str, provider: &str) -> String {
    format!(
        "crd_0{}",
        &format!("{:X}", Sha256::digest(format!("{node}\n{provider}")))[..25]
    )
}

/// Lists only the requesting user's Device projections. A cursor is tied to the
/// entire visible snapshot, actor, and repository; configuration changes require a refresh.
#[allow(clippy::too_many_lines)]
pub(crate) fn model_routes(
    storage: &mut SqliteStorage,
    query: &winwincode_api::generated::ModelRouteAvailabilityListQuery,
    now: &Instant,
) -> Result<winwincode_api::generated::ModelRouteAvailabilityListResultResponse, ProviderRelayError>
{
    use winwincode_api::generated::Actor;
    use winwincode_control_plane::{ProductSessionService, WorkerLaunchGrantService};
    let Actor::UserActor(actor) = &query.actor else {
        return Err(ProviderRelayError::Denied);
    };
    if !(1..=200).contains(&query.page.limit) {
        return Err(ProviderRelayError::Invalid);
    }
    let target = if let Some(session) = &query.parameters.product_session_id {
        let scope = repository_scope_key(&query.scope)?;
        let record = ProductSessionService::new(storage)
            .get(&scope, session)
            .map_err(|_| ProviderRelayError::Unavailable)?
            .ok_or(ProviderRelayError::Denied)?;
        if !matches!(record.owner_actor(), winwincode_storage::PublicEventActor::User { id } if id == &actor.id)
        {
            return Err(ProviderRelayError::Denied);
        }
        WorkerLaunchGrantService::new(storage)
            .newest_grant_for_product_session(&session.0)
            .map_err(|_| ProviderRelayError::Unavailable)?
            .map(|anchor| anchor.client_node_id)
    } else {
        None
    };
    let grants = AccessGrantService::new(storage)
        .active_grants_for_user(&actor.id.0)
        .map_err(|_| ProviderRelayError::Unavailable)?;
    let mut items = Vec::new();
    for grant in grants {
        if !grant.permissions.can_use()
            || grant.expires_at.is_some_and(|expiry| expiry.0 <= now.0)
            || target
                .as_ref()
                .is_some_and(|node| node != &grant.client_node_id)
        {
            continue;
        }
        let Some(node) = ClientRegistryService::new(storage)
            .snapshot(&grant.client_node_id)
            .map_err(|_| ProviderRelayError::Unavailable)?
        else {
            continue;
        };
        if node.presence_state == ClientPresenceState::Revoked {
            continue;
        }
        let Some(state) =
            storage.load_state(&format!("device-provider:{}", node.client_node_id))?
        else {
            continue;
        };
        let snapshot: winwincode_api::generated::DeviceProviderSnapshot =
            serde_json::from_slice(&state.payload)?;
        for provider in snapshot.providers {
            let (status, reason) = if !online(&node, now) {
                ("unknown", "runtime_status_unknown")
            } else if !provider.config.enabled {
                ("disabled", "provider_or_model_disabled")
            } else if !provider.credential_configured {
                ("auth_error", "credential_missing_or_revoked")
            } else {
                ("available", "ready")
            };
            for model in &provider.config.model_ids {
                items.push(json!({
                    "route":{"providerId":provider.config.provider_id, "modelId":model,
                        "credentialReferenceId":route_reference(&node.client_node_id, &provider.config.provider_id)},
                    "clientId":node.public_client_id, "providerDisplayName":format!("{} · {}", node.display_name, provider.config.display_name),
                    "modelDisplayName":model, "catalogSource":query.scope, "catalogVersion":snapshot.revision,
                    "providerVersion":snapshot.revision, "modelVersion":snapshot.revision,
                    "contextWindowTokens":0, "maxOutputTokens":8192, "toolSupport":"parallel", "reasoningEfforts":[],
                    "credentialRotationVersion":null, "isDefault":false, "status":status, "reason":reason
                }));
            }
        }
    }
    items.sort_by_key(|item| item["route"].to_string());
    let default = items.iter().position(|item| item["status"] == "available");
    if let Some(index) = default {
        items[index]["isDefault"] = json!(true);
    }
    let selected = default.map(|index| items[index]["route"].clone());
    let fingerprint = format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&(
            &query.actor,
            &query.scope,
            &query.parameters,
            &items
        ))?)
    );
    let offset = match &query.page.cursor {
        None => 0,
        Some(cursor) => {
            let (hash, offset) = cursor
                .0
                .split_once(':')
                .ok_or(ProviderRelayError::Invalid)?;
            if hash != fingerprint {
                return Err(ProviderRelayError::Conflict);
            }
            offset
                .parse::<usize>()
                .map_err(|_| ProviderRelayError::Invalid)?
        }
    };
    if offset > items.len() {
        return Err(ProviderRelayError::Invalid);
    }
    let end = offset
        .saturating_add(usize::try_from(query.page.limit).map_err(|_| ProviderRelayError::Invalid)?)
        .min(items.len());
    let ready = selected.is_some();
    Ok(serde_json::from_value(json!({
        "schemaVersion":"winwincode/v1", "query":"model.route.availability.list", "requestId":query.request_id,
        "page":{"hasMore":end<items.len(), "nextCursor":(end<items.len()).then(|| format!("{fingerprint}:{end}"))},
        "result":{"kind":"model_route_availability_page", "scope":query.scope,
            "settingsSource":null, "settingsRevision":null,
            "requestPoolSource":{"kind":"project", "organizationId":query.scope.organization_id,
                "workspaceId":query.scope.workspace_id, "projectId":query.scope.project_id},
            "requestPoolRevision":0, "defaultProviderId":selected.as_ref().map(|route| route["providerId"].clone()),
            "defaultModelId":selected.as_ref().map(|route| route["modelId"].clone()),
            "status":if ready {"available"} else {"unknown"},
            "reason":if ready {"ready"} else if items.is_empty() {"no_provider"} else {"runtime_status_unknown"},
            "items":items[offset..end]
        }
    }))?)
}

pub(crate) fn validate_session_route(
    storage: &mut SqliteStorage,
    scope: &winwincode_domain::RepositoryScope,
    session: &winwincode_domain::ProductSessionId,
    node: &str,
) -> Result<(), ProviderRelayError> {
    let scope = repository_scope_key(scope)?;
    let record = winwincode_control_plane::ProductSessionService::new(storage)
        .get(&scope, session)
        .map_err(|_| ProviderRelayError::Unavailable)?
        .ok_or(ProviderRelayError::Denied)?;
    let route = record.model_route();
    if route.credential_reference_id.0 != route_reference(node, &route.provider_id) {
        return Err(ProviderRelayError::Denied);
    }
    let state = storage
        .load_state(&format!("device-provider:{node}"))?
        .ok_or(ProviderRelayError::Unavailable)?;
    let snapshot: winwincode_api::generated::DeviceProviderSnapshot =
        serde_json::from_slice(&state.payload)?;
    if !snapshot.providers.iter().any(|provider| {
        provider.config.provider_id == route.provider_id
            && provider.config.enabled
            && provider.credential_configured
            && provider.config.model_ids.contains(&route.model_id)
    }) {
        return Err(ProviderRelayError::Conflict);
    }
    Ok(())
}

pub(crate) fn repository_scope_key(
    scope: &winwincode_domain::RepositoryScope,
) -> Result<ReceiptScopeKey, winwincode_storage::StorageError> {
    winwincode_storage::receipt_scope_key(&winwincode_storage::PublicEventScope::Repository {
        organization_id: scope.organization_id.clone(),
        workspace_id: scope.workspace_id.clone(),
        project_id: scope.project_id.clone(),
        repository_id: scope.repository_id.clone(),
    })
}
