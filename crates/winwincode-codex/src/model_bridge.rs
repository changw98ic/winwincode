// SPDX-License-Identifier: Apache-2.0

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::future::Future;
use std::sync::{Arc, Mutex, MutexGuard, RwLock};

use crate::outbox::ExecutionOutbox;
use crate::{
    WorkerExecutionPort,
    action_bridge::ExecutionPortActionGate,
    authority_clock::{AuthorityClock, AuthorityClockError, canonical_instant},
    model_port_client::{
        ModelAuthorityRejection, ModelChunkDelivery, ModelChunkDisposition, ModelChunkSink,
        ModelCursorStore, ModelLeaseAuthority, ModelLeaseAuthoritySource, ModelMessageMetadata,
        ModelPortClientErrorCode, ModelSinkDeliveryStatus, ModelTerminationReason,
        OpenModelExchangeCommand, WorkerModelPortClient, chunk_termination,
    },
};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use futures::{future::BoxFuture, stream};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::sync::{Mutex as AsyncMutex, mpsc};
use winwincode_domain::{CodexThreadId, ExecutionMessageId, Instant, ModelExchangeId, RequestId};
use winwincode_execution_port::{
    generated::{
        EncodedPayload, ExecutionPortError, ExecutionPortMessage, ModelChunkMessage,
        ModelGatewayRoute,
    },
    replay::{ReplayAuthority, ReplayStreamKey},
    runtime_replay::RuntimeReplayIdentity,
    typed_replay::frame_from_message,
};
use winwincode_kernel::{ModelPort, ModelPortFailure, ModelPortRequest, ModelPortStream};

use crate::performance::{
    PerformanceOperationCompletion, PerformanceOperationKind, PrimaryModelUsage,
};
use crate::store::{AdapterStore, ModelCallPhase};

type ModelClient =
    WorkerModelPortClient<QueuedModelPort, AdapterStore, SharedAuthoritySource, KernelStreamSink>;

#[derive(Clone)]
pub(crate) struct ModelRunBinding {
    pub run_key: String,
    pub canonical_thread_id: CodexThreadId,
    pub kernel_session_id: String,
    pub authority: ModelLeaseAuthority,
    pub opened_at: Instant,
}

#[derive(Clone, Default)]
pub(crate) struct SharedAuthoritySource {
    current: Arc<RwLock<HashMap<String, ModelLeaseAuthority>>>,
    lineage: Arc<RwLock<HashMap<String, ModelLeaseAuthority>>>,
    store: Option<AdapterStore>,
    authority_clock: AuthorityClock,
}

impl SharedAuthoritySource {
    pub(crate) fn with_store(mut self, store: AdapterStore) -> Self {
        self.store = Some(store);
        self
    }

    pub(crate) fn update_now(&self, now: &Instant) -> Result<(), BridgeError> {
        self.authority_clock
            .observe(now)
            .map_err(|error| match error {
                AuthorityClockError::Unavailable => BridgeError::Unavailable,
                AuthorityClockError::InvalidTime => BridgeError::StaleAuthority,
            })
    }

    pub(crate) fn observed_now(&self) -> Result<Option<Instant>, BridgeError> {
        self.authority_clock.now().map_err(|error| match error {
            AuthorityClockError::Unavailable => BridgeError::Unavailable,
            AuthorityClockError::InvalidTime => BridgeError::StaleAuthority,
        })
    }

    pub(crate) fn install(
        &self,
        run_key: &str,
        authority: ModelLeaseAuthority,
    ) -> Result<(), BridgeError> {
        if run_key.trim().is_empty() {
            return Err(BridgeError::Conflict);
        }
        let job_id = authority.lease.job_id.0.clone();
        let thread_id = authority.session_identity.codex_thread_id.0.clone();
        let process_root_same = self
            .read_authorities()?
            .get(&job_id)
            .is_some_and(|existing| same_binding_with_extended_lease(existing, &authority));
        let persisted_root_same =
            self.load_lineage_binding(&thread_id)?
                .is_some_and(|(existing_run_key, existing)| {
                    existing_run_key == run_key
                        && same_binding_with_extended_lease(&existing, &authority)
                });
        let same_root = if self.store.is_some() {
            persisted_root_same
        } else {
            process_root_same
        };
        self.persist_root(run_key, &thread_id, &authority)?;
        self.update_now(&authority.lease.issued_at)?;
        self.write_authorities()?
            .insert(job_id.clone(), authority.clone());
        let mut lineage = self.write_lineage()?;
        if same_root {
            lineage.retain(|_, candidate| {
                if candidate.lease.job_id.0 != job_id {
                    return true;
                }
                let mut renewed = candidate.clone();
                renewed.lease.expires_at = authority.lease.expires_at.clone();
                if !common_authority(&renewed, &authority) {
                    return false;
                }
                *candidate = renewed;
                true
            });
        } else {
            lineage.retain(|_, candidate| candidate.lease.job_id.0 != job_id);
        }
        lineage.insert(
            authority.session_identity.codex_thread_id.0.clone(),
            authority,
        );
        Ok(())
    }

    pub(crate) fn install_child(
        &self,
        run_key: &str,
        authority: ModelLeaseAuthority,
    ) -> Result<(), BridgeError> {
        if run_key.trim().is_empty() {
            return Err(BridgeError::Conflict);
        }
        let process_root = self
            .read_authorities()?
            .get(&authority.lease.job_id.0)
            .cloned()
            .filter(|root| common_authority(root, &authority));
        let durable_matches = self
            .load_lineage_for_run(run_key)?
            .into_iter()
            .any(|root| common_authority(&root, &authority));
        if (process_root.is_none() || self.store.is_some()) && !durable_matches {
            return Err(BridgeError::StaleAuthority);
        }
        let thread_id = authority.session_identity.codex_thread_id.0.clone();
        let mut lineage = self.write_lineage()?;
        if let Some(existing) = lineage.get(&thread_id)
            && existing != &authority
        {
            return Err(BridgeError::Conflict);
        }
        self.persist_child(run_key, &thread_id, &authority)?;
        lineage.insert(thread_id, authority);
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn remove(&self, job_id: &str, run_key: &str) -> Result<(), BridgeError> {
        if let Some(store) = &self.store {
            store
                .remove_model_thread_lineage(run_key)
                .map_err(|_| BridgeError::Unavailable)?;
        }
        self.write_authorities()?.remove(job_id);
        self.write_lineage()?
            .retain(|_, authority| authority.lease.job_id.0 != job_id);
        Ok(())
    }

    fn read_authorities(
        &self,
    ) -> Result<std::sync::RwLockReadGuard<'_, HashMap<String, ModelLeaseAuthority>>, BridgeError>
    {
        self.current.read().map_err(|_| BridgeError::Unavailable)
    }

    fn write_authorities(
        &self,
    ) -> Result<std::sync::RwLockWriteGuard<'_, HashMap<String, ModelLeaseAuthority>>, BridgeError>
    {
        self.current.write().map_err(|_| BridgeError::Unavailable)
    }

    fn write_lineage(
        &self,
    ) -> Result<std::sync::RwLockWriteGuard<'_, HashMap<String, ModelLeaseAuthority>>, BridgeError>
    {
        self.lineage.write().map_err(|_| BridgeError::Unavailable)
    }

    fn persist_root(
        &self,
        run_key: &str,
        thread_id: &str,
        authority: &ModelLeaseAuthority,
    ) -> Result<(), BridgeError> {
        let Some(store) = &self.store else {
            return Ok(());
        };
        let bytes = serde_json::to_vec(authority).map_err(|_| BridgeError::InvalidPayload)?;
        store
            .replace_model_thread_lineage_for_job(
                run_key,
                thread_id,
                &authority.lease.job_id.0,
                &bytes,
            )
            .map_err(|_| BridgeError::Unavailable)
    }

    fn persist_child(
        &self,
        run_key: &str,
        thread_id: &str,
        authority: &ModelLeaseAuthority,
    ) -> Result<(), BridgeError> {
        let Some(store) = &self.store else {
            return Ok(());
        };
        let bytes = serde_json::to_vec(authority).map_err(|_| BridgeError::InvalidPayload)?;
        store
            .retain_model_thread_lineage(run_key, thread_id, &bytes)
            .map_err(|error| match error {
                crate::store::AdapterStoreError::Conflict => BridgeError::Conflict,
                crate::store::AdapterStoreError::Unavailable
                | crate::store::AdapterStoreError::Corrupt => BridgeError::Unavailable,
            })
    }

    fn load_lineage(&self, thread_id: &str) -> Result<Option<ModelLeaseAuthority>, BridgeError> {
        Ok(self
            .load_lineage_binding(thread_id)?
            .map(|(_, authority)| authority))
    }

    fn load_lineage_binding(
        &self,
        thread_id: &str,
    ) -> Result<Option<(String, ModelLeaseAuthority)>, BridgeError> {
        let Some(store) = &self.store else {
            return Ok(None);
        };
        let Some((run_key, bytes)) = store
            .load_model_thread_lineage(thread_id)
            .map_err(|_| BridgeError::Unavailable)?
        else {
            return Ok(None);
        };
        serde_json::from_slice(&bytes)
            .map(|authority| Some((run_key, authority)))
            .map_err(|_| BridgeError::Unavailable)
    }

    fn load_lineage_for_run(&self, run_key: &str) -> Result<Vec<ModelLeaseAuthority>, BridgeError> {
        let Some(store) = &self.store else {
            return Ok(Vec::new());
        };
        store
            .load_model_thread_lineage_for_run(run_key)
            .map_err(|_| BridgeError::Unavailable)?
            .into_iter()
            .map(|(_, bytes)| serde_json::from_slice(&bytes).map_err(|_| BridgeError::Unavailable))
            .collect()
    }
    fn original_request_authority(
        &self,
        authority: &ModelLeaseAuthority,
        open: Option<&winwincode_execution_port::generated::ModelOpenMessage>,
    ) -> Result<ModelLeaseAuthority, ModelAuthorityRejection> {
        let thread = &authority.session_identity.codex_thread_id.0;
        let cached = self
            .lineage
            .read()
            .map_err(|_| ModelAuthorityRejection::Unavailable)?
            .get(thread)
            .cloned();
        let expected = match cached {
            Some(expected) => expected,
            None => self
                .load_lineage(thread)
                .map_err(|_| ModelAuthorityRejection::Unavailable)?
                .ok_or(ModelAuthorityRejection::StaleLease)?,
        };
        if !same_binding_with_extended_lease(authority, &expected) {
            return Err(ModelAuthorityRejection::StaleLease);
        }
        if let Some(open) = open {
            if open.lease != authority.lease
                || open.worker_session_id != authority.worker_session_id
                || open.session_identity != authority.session_identity
            {
                return Err(ModelAuthorityRejection::StaleLease);
            }
        } else if expected != *authority {
            return Err(ModelAuthorityRejection::StaleLease);
        }
        Ok(expected)
    }

    fn validate_original_request(
        &self,
        authority: &ModelLeaseAuthority,
        open: Option<&winwincode_execution_port::generated::ModelOpenMessage>,
        now: &Instant,
    ) -> Result<(), ModelAuthorityRejection> {
        let expected = self.original_request_authority(authority, open)?;
        self.validate_current(&expected, now)
    }
}

impl ModelLeaseAuthoritySource for SharedAuthoritySource {
    fn validate_retained_exchange(
        &self,
        authority: &ModelLeaseAuthority,
        exchange: &ModelExchangeId,
        now: &Instant,
    ) -> Result<(), ModelAuthorityRejection> {
        if !canonical_instant(now) {
            return Err(ModelAuthorityRejection::StaleLease);
        }
        let open = self
            .store
            .as_ref()
            .map(|store| ExecutionOutbox::read_model_open(store, exchange))
            .transpose()
            .map_err(|_| ModelAuthorityRejection::Unavailable)?
            .flatten();
        if let Some(open) = open {
            self.original_request_authority(authority, Some(&open))?;
            return Ok(());
        }
        self.validate_original_request(authority, None, now)
    }

    fn validate_exchange(
        &self,
        authority: &ModelLeaseAuthority,
        exchange: &ModelExchangeId,
        now: &Instant,
    ) -> Result<(), ModelAuthorityRejection> {
        let open = self
            .store
            .as_ref()
            .map(|store| ExecutionOutbox::read_model_open(store, exchange))
            .transpose()
            .map_err(|_| ModelAuthorityRejection::Unavailable)?
            .flatten();
        self.validate_original_request(authority, open.as_ref(), now)
    }

    fn validate_current(
        &self,
        authority: &ModelLeaseAuthority,
        now: &Instant,
    ) -> Result<(), ModelAuthorityRejection> {
        let lineage = self
            .lineage
            .read()
            .map_err(|_| ModelAuthorityRejection::Unavailable)?;
        let thread_id = &authority.session_identity.codex_thread_id.0;
        let expected = if let Some(expected) = lineage.get(thread_id).cloned() {
            Some(expected)
        } else {
            self.load_lineage(thread_id)
                .map_err(|_| ModelAuthorityRejection::Unavailable)?
        };
        let Some(expected) = expected.as_ref() else {
            return Err(ModelAuthorityRejection::Unavailable);
        };
        if expected != authority
            || expected.lease.job_id != authority.lease.job_id
            || expected.session_identity.codex_thread_id
                != authority.session_identity.codex_thread_id
        {
            return Err(ModelAuthorityRejection::StaleLease);
        }
        self.update_now(now).map_err(|error| match error {
            BridgeError::Unavailable => ModelAuthorityRejection::Unavailable,
            _ => ModelAuthorityRejection::StaleLease,
        })?;
        let trusted_now = self
            .observed_now()
            .map_err(|_| ModelAuthorityRejection::Unavailable)?
            .ok_or(ModelAuthorityRejection::Unavailable)?;
        if trusted_now.0 < authority.lease.issued_at.0
            || trusted_now.0 >= authority.lease.expires_at.0
        {
            return Err(ModelAuthorityRejection::ExpiredLease);
        }
        Ok(())
    }
}

impl ReplayAuthority for SharedAuthoritySource {
    type Context = RuntimeReplayIdentity;
    type Error = BridgeError;

    fn validate_active_lease(
        &self,
        stream: &ReplayStreamKey,
        context: &Self::Context,
    ) -> Result<(), Self::Error> {
        if &context.stream_key() != stream {
            return Err(BridgeError::StaleAuthority);
        }
        let expected = {
            let lineage = self.lineage.read().map_err(|_| BridgeError::Unavailable)?;
            if let Some(expected) = lineage
                .get(&context.session_identity.codex_thread_id.0)
                .cloned()
            {
                Some(expected)
            } else {
                drop(lineage);
                self.load_lineage(&context.session_identity.codex_thread_id.0)?
            }
        };
        let Some(expected) = expected else {
            return Err(BridgeError::StaleAuthority);
        };
        let presented = ModelLeaseAuthority {
            lease: context.lease.clone(),
            worker_session_id: context.worker_session_id.clone(),
            session_identity: context.session_identity.clone(),
        };
        if !(expected == presented
            || (winwincode_execution_port::execution_identity::retained_lease_matches_current(
                &presented.lease,
                &expected.lease,
            ) && same_binding_with_extended_lease(&presented, &expected)))
            || context.codex_thread_id != context.session_identity.codex_thread_id
            || expected.session_identity.codex_thread_id != context.codex_thread_id
        {
            return Err(BridgeError::StaleAuthority);
        }
        Ok(())
    }
}

#[derive(Clone, Default)]
struct QueuedModelPort {
    messages: Arc<Mutex<VecDeque<ExecutionPortMessage>>>,
}

impl QueuedModelPort {
    fn take(&self) -> Result<Vec<ExecutionPortMessage>, BridgeError> {
        Ok(self.lock()?.drain(..).collect())
    }

    fn discard_for_thread(&self, thread_id: &CodexThreadId) -> Result<(), BridgeError> {
        self.lock()?.retain(|message| match message {
            ExecutionPortMessage::ModelOpenMessage(message) => {
                message.session_identity.codex_thread_id != *thread_id
            }
            ExecutionPortMessage::ModelAckMessage(message) => {
                message.session_identity.codex_thread_id != *thread_id
            }
            _ => true,
        });
        Ok(())
    }

    fn lock(&self) -> Result<MutexGuard<'_, VecDeque<ExecutionPortMessage>>, BridgeError> {
        self.messages.lock().map_err(|_| BridgeError::Unavailable)
    }
}

impl WorkerExecutionPort for QueuedModelPort {
    type Error = BridgeError;

    fn send(
        &mut self,
        message: ExecutionPortMessage,
    ) -> impl Future<Output = Result<(), Self::Error>> {
        let result = self.lock().map(|mut messages| messages.push_back(message));
        std::future::ready(result)
    }
}

type KernelStreamItem = Result<String, ModelPortFailure>;

#[derive(Clone, Default)]
struct KernelStreamSink {
    streams: Arc<Mutex<HashMap<String, mpsc::UnboundedSender<KernelStreamItem>>>>,
}

impl KernelStreamSink {
    fn register(
        &self,
        exchange: &ModelExchangeId,
    ) -> Result<mpsc::UnboundedReceiver<KernelStreamItem>, BridgeError> {
        let (sender, receiver) = mpsc::unbounded_channel();
        let mut streams = self.lock()?;
        streams.insert(exchange.0.clone(), sender);
        Ok(receiver)
    }

    fn remove(&self, exchange: &ModelExchangeId) -> Result<(), BridgeError> {
        self.lock()?.remove(&exchange.0);
        Ok(())
    }

    fn deliver_item(
        &self,
        exchange: &ModelExchangeId,
        item: KernelStreamItem,
    ) -> Result<(), BridgeError> {
        let sender = self
            .lock()?
            .get(&exchange.0)
            .cloned()
            .ok_or(BridgeError::UnknownExchange)?;
        sender.send(item).map_err(|_| BridgeError::UnknownExchange)
    }

    fn lock(
        &self,
    ) -> Result<MutexGuard<'_, HashMap<String, mpsc::UnboundedSender<KernelStreamItem>>>, BridgeError>
    {
        self.streams.lock().map_err(|_| BridgeError::Unavailable)
    }
}

impl ModelChunkSink for KernelStreamSink {
    type Error = BridgeError;

    async fn deliver(
        &mut self,
        delivery: ModelChunkDelivery<'_>,
    ) -> Result<ModelSinkDeliveryStatus, Self::Error> {
        let item = if let Some(error) = delivery.error {
            Err(decode_provider_failure(error, delivery.payload)?)
        } else if let Some(payload) = delivery.payload {
            if payload.content_type != "application/json" {
                return Err(BridgeError::InvalidPayload);
            }
            let bytes = STANDARD
                .decode(&payload.data_base64)
                .map_err(|_| BridgeError::InvalidPayload)?;
            let digest = format!("sha256:{:x}", Sha256::digest(&bytes));
            if digest != payload.payload_digest.0 {
                return Err(BridgeError::InvalidPayload);
            }
            Ok(String::from_utf8(bytes).map_err(|_| BridgeError::InvalidPayload)?)
        } else if delivery.is_final {
            return Ok(ModelSinkDeliveryStatus::Applied);
        } else {
            return Err(BridgeError::InvalidPayload);
        };
        let sender = self
            .lock()?
            .get(&delivery.model_exchange_id.0)
            .cloned()
            .ok_or(BridgeError::UnknownExchange)?;
        sender
            .send(item)
            .map_err(|_| BridgeError::UnknownExchange)?;
        Ok(ModelSinkDeliveryStatus::Applied)
    }

    async fn terminate(
        &mut self,
        model_exchange_id: &ModelExchangeId,
        reason: ModelTerminationReason,
    ) -> Result<(), Self::Error> {
        if let Some(sender) = self.lock()?.remove(&model_exchange_id.0) {
            let _ = sender.send(Err(ModelPortFailure::new(
                "MODEL_STREAM_TERMINATED",
                format!("model stream terminated: {reason:?}"),
            )));
        }
        Ok(())
    }

    async fn release(&mut self, model_exchange_id: &ModelExchangeId) -> Result<(), Self::Error> {
        self.remove(model_exchange_id)
    }
}

pub(crate) struct ExecutionPortModelBridge {
    route: ModelGatewayRoute,
    expected_provider: String,
    device_provider_directory: std::sync::OnceLock<std::path::PathBuf>,
    queue: QueuedModelPort,
    sink: KernelStreamSink,
    authority: SharedAuthoritySource,
    client: AsyncMutex<ModelClient>,
    ordinal_store: AdapterStore,
    outbox: ExecutionOutbox,
    bindings: RwLock<HashMap<String, ModelRunBinding>>,
    exchanges: Mutex<HashMap<String, ModelExchangeOwner>>,
    action_gate: RwLock<Option<Arc<ExecutionPortActionGate>>>,
}

#[derive(Clone)]
struct ModelExchangeOwner {
    thread_id: CodexThreadId,
    run_key: String,
    model_call_id: String,
}

struct OpenStreamExchange {
    binding: ModelRunBinding,
    payload_bytes: Vec<u8>,
    request_digest: winwincode_domain::Sha256Digest,
    model_call_id: String,
    exchange_identity: String,
    exchange: ModelExchangeId,
}

enum OpenStreamPreparation {
    Replay(ModelPortStream),
    Open(Box<OpenStreamExchange>),
}

impl ExecutionPortModelBridge {
    pub(crate) fn new(
        store: AdapterStore,
        outbox: ExecutionOutbox,
        route: ModelGatewayRoute,
        expected_provider: String,
        authority: SharedAuthoritySource,
    ) -> Self {
        let queue = QueuedModelPort::default();
        let sink = KernelStreamSink::default();
        let authority = authority.with_store(store.clone());
        let client = WorkerModelPortClient::new(
            queue.clone(),
            store.clone(),
            authority.clone(),
            sink.clone(),
        );
        Self {
            route,
            expected_provider,
            device_provider_directory: std::sync::OnceLock::new(),
            queue,
            sink,
            authority,
            client: AsyncMutex::new(client),
            ordinal_store: store,
            outbox,
            bindings: RwLock::new(HashMap::new()),
            exchanges: Mutex::new(HashMap::new()),
            action_gate: RwLock::new(None),
        }
    }

    pub(crate) fn attach_device_provider_directory(
        &self,
        directory: std::path::PathBuf,
    ) -> Result<(), BridgeError> {
        if self.device_provider_directory.get() == Some(&directory) {
            return Ok(());
        }
        self.device_provider_directory
            .set(directory)
            .map_err(|_| BridgeError::Conflict)
    }

    pub(crate) fn attach_action_gate(
        &self,
        action_gate: Arc<ExecutionPortActionGate>,
    ) -> Result<(), BridgeError> {
        let mut installed = self
            .action_gate
            .write()
            .map_err(|_| BridgeError::Unavailable)?;
        if installed
            .as_ref()
            .is_some_and(|current| !Arc::ptr_eq(current, &action_gate))
        {
            return Err(BridgeError::Conflict);
        }
        *installed = Some(action_gate);
        Ok(())
    }

    fn sync_action_binding(&self, binding: &ModelRunBinding) -> Result<(), BridgeError> {
        let action_gate = self
            .action_gate
            .read()
            .map_err(|_| BridgeError::Unavailable)?
            .clone();
        if let Some(action_gate) = action_gate {
            action_gate
                .install_child_binding(binding.clone())
                .map_err(|error| match error {
                    crate::action_bridge::ActionBridgeError::Conflict => BridgeError::Conflict,
                    _ => BridgeError::Unavailable,
                })?;
        }
        Ok(())
    }

    fn sync_action_clock(&self, now: &Instant) -> Result<(), BridgeError> {
        let action_gate = self
            .action_gate
            .read()
            .map_err(|_| BridgeError::Unavailable)?
            .clone();
        if let Some(action_gate) = action_gate {
            action_gate
                .update_now(now)
                .map_err(|_| BridgeError::Unavailable)?;
        }
        Ok(())
    }

    pub(crate) fn authority(&self) -> SharedAuthoritySource {
        self.authority.clone()
    }

    pub(crate) fn model_port(self: &Arc<Self>) -> Arc<dyn ModelPort> {
        Arc::new(KernelModelPort {
            bridge: Arc::clone(self),
            fusion_member: None,
        })
    }

    pub(crate) fn fusion_model_port(self: &Arc<Self>, member: String) -> Arc<dyn ModelPort> {
        Arc::new(KernelModelPort {
            bridge: Arc::clone(self),
            fusion_member: Some(member),
        })
    }

    pub(crate) fn task_context_overhead_bytes(
        &self,
        run_key: &str,
    ) -> Result<usize, ModelPortFailure> {
        let run = self
            .ordinal_store
            .load_run_header::<serde_json::Value>(run_key)
            .map_err(model_store_failure)?
            .ok_or_else(|| model_failure(BridgeError::InvalidPayload))?;
        crate::task_handoff::context_snapshot(&run)
            .map(|context| context.map_or(0, |text| text.len()))
            .map_err(|_| model_failure(BridgeError::InvalidPayload))
    }

    pub(crate) fn install_binding(&self, binding: ModelRunBinding) -> Result<(), BridgeError> {
        self.authority
            .install(&binding.run_key, binding.authority.clone())?;
        self.insert_binding(binding, true)
    }

    pub(crate) fn install_child_binding(
        &self,
        binding: ModelRunBinding,
    ) -> Result<(), BridgeError> {
        self.authority
            .install_child(&binding.run_key, binding.authority.clone())?;
        self.insert_binding(binding, false)
    }

    fn insert_binding(
        &self,
        binding: ModelRunBinding,
        replace_fenced_run: bool,
    ) -> Result<(), BridgeError> {
        let mut bindings = self
            .bindings
            .write()
            .map_err(|_| BridgeError::Unavailable)?;
        if replace_fenced_run && extend_binding_lineage(&mut bindings, &binding) {
            return Ok(());
        }
        let fence_lineage = replace_fenced_run
            && !bindings
                .get(&binding.canonical_thread_id.0)
                .is_some_and(|existing| same_binding(existing, &binding));
        for alias in [&binding.kernel_session_id, &binding.canonical_thread_id.0] {
            if let Some(existing) = bindings.get(alias)
                && !same_binding(existing, &binding)
                && (!fence_lineage
                    || existing.authority.lease.job_id != binding.authority.lease.job_id)
            {
                return Err(BridgeError::Conflict);
            }
        }
        if fence_lineage {
            let job_id = binding.authority.lease.job_id.clone();
            bindings.retain(|_, existing| {
                existing.authority.lease.job_id != job_id || same_binding(existing, &binding)
            });
        }
        bindings.insert(binding.kernel_session_id.clone(), binding.clone());
        bindings.insert(binding.canonical_thread_id.0.clone(), binding);
        Ok(())
    }

    pub(crate) fn detach_binding(&self, binding: &ModelRunBinding) -> Result<(), BridgeError> {
        self.bindings
            .write()
            .map_err(|_| BridgeError::Unavailable)?
            .retain(|_, candidate| candidate.run_key != binding.run_key);
        Ok(())
    }

    fn mark_provider_final(
        &self,
        owner: &ModelExchangeOwner,
        jev_complete: bool,
    ) -> Result<(), BridgeError> {
        self.ordinal_store
            .mark_model_call_provider_final_with_accounting(
                &owner.run_key,
                &owner.model_call_id,
                jev_complete,
            )
            .map_err(|error| match error {
                crate::store::AdapterStoreError::Conflict => BridgeError::Conflict,
                crate::store::AdapterStoreError::Unavailable
                | crate::store::AdapterStoreError::Corrupt => BridgeError::Unavailable,
            })
    }

    fn retain_and_forward_frame(
        &self,
        chunk: &ModelChunkMessage,
        owner: &ModelExchangeOwner,
    ) -> Result<(), BridgeError> {
        validate_chunk_for_retention(chunk)
            .inspect_err(|error| log_model_intake_failure(chunk, "retention", *error))?;
        self.ordinal_store
            .retain_model_call_frame(&owner.run_key, &owner.model_call_id, chunk)
            .map_err(model_frame_store_error)?;
        if let Some(error) = &chunk.error {
            let _ = self.sink.deliver_item(
                &chunk.model_exchange_id,
                Err(decode_provider_failure(error, chunk.payload.as_ref())?),
            );
        } else if let Some(payload) = &chunk.payload {
            let bytes = STANDARD
                .decode(&payload.data_base64)
                .map_err(|_| BridgeError::InvalidPayload)?;
            let text = String::from_utf8(bytes).map_err(|_| BridgeError::InvalidPayload)?;
            let _ = self.sink.deliver_item(&chunk.model_exchange_id, Ok(text));
        }
        Ok(())
    }

    async fn reserve_and_accept(
        &self,
        client: &mut ModelClient,
        chunk: &ModelChunkMessage,
        owner: &ModelExchangeOwner,
        metadata: ModelMessageMetadata,
        exact_duplicate: bool,
        contiguous_new: bool,
    ) -> Result<(ModelChunkDisposition, bool), BridgeError> {
        let pre_retained = if exact_duplicate {
            true
        } else if contiguous_new {
            validate_chunk_for_retention(chunk)?;
            self.ordinal_store
                .retain_model_call_frame(&owner.run_key, &owner.model_call_id, chunk)
                .map_err(model_frame_store_error)?;
            true
        } else {
            false
        };
        let disposition = match client.accept_chunk(chunk, metadata).await {
            Ok(disposition) => disposition,
            Err(_) if exact_duplicate => ModelChunkDisposition::Duplicate {
                confirmed_sequence: u64::try_from(chunk.sequence.0)
                    .map_err(|_| BridgeError::InvalidPayload)?,
            },
            Err(_) if pre_retained => ModelChunkDisposition::Delivered {
                confirmed_sequence: u64::try_from(chunk.sequence.0)
                    .map_err(|_| BridgeError::InvalidPayload)?,
                termination: chunk_termination(chunk),
            },
            Err(_) => return Err(BridgeError::Protocol),
        };
        Ok((disposition, pre_retained))
    }

    #[allow(
        clippy::too_many_lines,
        reason = "Authority, durable frame and live or detached stream intake share one checked envelope"
    )]
    pub(crate) async fn accept_chunk(
        &self,
        chunk: &ModelChunkMessage,
        received_at: &Instant,
    ) -> Result<ModelChunkDisposition, BridgeError> {
        let (owner, binding, process_exchange) = self
            .resolve_chunk_owner(chunk)
            .inspect_err(|error| log_model_intake_failure(chunk, "resolve", *error))?;
        let metadata = ModelMessageMetadata {
            message_id: execution_message_id(b"model-ack", &chunk.message_id.0),
            sent_at: received_at.clone(),
        };
        self.validate_chunk_authority(chunk, &binding, received_at)
            .inspect_err(|error| log_model_intake_failure(chunk, "authority", *error))?;
        self.sync_action_clock(received_at)?;
        let mut client = self.client.lock().await;
        let (mut exact_duplicate, contiguous_new, provider_final) = self
            .ordinal_store
            .model_call_frame_position(&owner.run_key, &owner.model_call_id, chunk)
            .map_err(model_frame_store_error)?;
        if provider_final && !exact_duplicate {
            return Err(BridgeError::Conflict);
        }
        let detached_delivery = !process_exchange && contiguous_new && !exact_duplicate;
        if !process_exchange && chunk.is_final && (exact_duplicate || contiguous_new) {
            if detached_delivery {
                self.retain_and_forward_frame(chunk, &owner)?;
            }
            self.restore_retained_cursor(chunk, &owner)?;
            exact_duplicate = true;
        }
        if !process_exchange {
            if exact_duplicate && chunk.is_final {
                let disposition = client
                    .accept_terminal_duplicate(
                        ModelLeaseAuthority {
                            lease: chunk.lease.clone(),
                            worker_session_id: chunk.worker_session_id.clone(),
                            session_identity: chunk.session_identity.clone(),
                        },
                        chunk,
                        metadata,
                    )
                    .await
                    .map_err(|_| BridgeError::Protocol)?;
                self.retain_provider_final_frame(&owner, chunk, received_at)?;
                return Ok(if detached_delivery {
                    ModelChunkDisposition::Delivered {
                        confirmed_sequence: u64::try_from(chunk.sequence.0)
                            .map_err(|_| BridgeError::InvalidPayload)?,
                        termination: chunk_termination(chunk),
                    }
                } else {
                    disposition
                });
            }
            if exact_duplicate {
                return Ok(ModelChunkDisposition::Duplicate {
                    confirmed_sequence: u64::try_from(chunk.sequence.0)
                        .map_err(|_| BridgeError::InvalidPayload)?,
                });
            }
            if contiguous_new {
                self.retain_and_forward_frame(chunk, &owner)?;
                if chunk.is_final {
                    self.retain_provider_final_frame(&owner, chunk, received_at)?;
                }
                return Ok(ModelChunkDisposition::Delivered {
                    confirmed_sequence: u64::try_from(chunk.sequence.0)
                        .map_err(|_| BridgeError::InvalidPayload)?,
                    termination: chunk_termination(chunk),
                });
            }
            return Err(BridgeError::Conflict);
        }
        let (disposition, pre_retained) = self
            .reserve_and_accept(
                &mut client,
                chunk,
                &owner,
                metadata,
                exact_duplicate,
                contiguous_new,
            )
            .await?;
        drop(client);
        if !pre_retained
            && matches!(
                disposition,
                ModelChunkDisposition::Delivered { .. } | ModelChunkDisposition::Duplicate { .. }
            )
        {
            self.ordinal_store
                .retain_model_call_frame(&owner.run_key, &owner.model_call_id, chunk)
                .map_err(model_frame_store_error)?;
        }
        let terminal = matches!(
            disposition,
            ModelChunkDisposition::Delivered {
                termination: Some(_),
                ..
            }
        ) || matches!(disposition, ModelChunkDisposition::Duplicate { .. })
            && chunk.is_final;
        if terminal {
            self.retain_provider_final_frame(&owner, chunk, received_at)?;
            self.client
                .lock()
                .await
                .release_terminal(&chunk.model_exchange_id)
                .await
                .map_err(|_| BridgeError::Protocol)?;
            self.exchanges
                .lock()
                .map_err(|_| BridgeError::Unavailable)?
                .remove(&chunk.model_exchange_id.0);
        }
        Ok(disposition)
    }

    fn restore_retained_cursor(
        &self,
        chunk: &ModelChunkMessage,
        owner: &ModelExchangeOwner,
    ) -> Result<(), BridgeError> {
        validate_chunk_for_retention(chunk)?;
        self.ordinal_store
            .retain_model_call_frame(&owner.run_key, &owner.model_call_id, chunk)
            .map_err(model_frame_store_error)?;
        let mapped = frame_from_message(&ExecutionPortMessage::ModelChunkMessage(chunk.clone()))
            .map_err(|_| BridgeError::InvalidPayload)?;
        let mut store = self.ordinal_store.clone();
        let head = ModelCursorStore::load_head(&mut store, &mapped.stream)
            .map_err(model_frame_store_error)?;
        let frames = self
            .ordinal_store
            .load_model_call_frames_after(
                &owner.run_key,
                &owner.model_call_id,
                head.confirmed_sequence,
            )
            .map_err(model_frame_store_error)?;
        let mut confirmed = head.confirmed_sequence;
        for frame in frames {
            let mapped =
                frame_from_message(&ExecutionPortMessage::ModelChunkMessage(frame.clone()))
                    .map_err(|_| BridgeError::InvalidPayload)?;
            let fingerprint = crate::model_port_client::ModelChunkFingerprint {
                sequence: u64::try_from(frame.sequence.0)
                    .map_err(|_| BridgeError::InvalidPayload)?,
                message_id: frame.message_id.clone(),
                digest: winwincode_domain::Sha256Digest(mapped.frame.digest),
                is_final: frame.is_final,
                has_error: frame.error.is_some(),
            };
            ModelCursorStore::record_delivery(
                &mut store,
                &mapped.stream,
                confirmed,
                &fingerprint,
                chunk_termination(&frame),
            )
            .map_err(model_frame_store_error)?;
            confirmed = fingerprint.sequence;
        }
        Ok(())
    }

    fn retain_provider_final_frame(
        &self,
        owner: &ModelExchangeOwner,
        chunk: &ModelChunkMessage,
        received_at: &Instant,
    ) -> Result<(), BridgeError> {
        let jev_complete = self.record_primary_model_completion(owner, chunk, received_at);
        self.ordinal_store
            .seal_model_call_response(&owner.run_key, &owner.model_call_id)
            .map_err(model_frame_store_error)?;
        self.mark_provider_final(owner, jev_complete)
    }

    fn record_primary_model_completion(
        &self,
        owner: &ModelExchangeOwner,
        chunk: &ModelChunkMessage,
        received_at: &Instant,
    ) -> bool {
        let jev_complete = self
            .retain_device_jev_receipts(owner, &chunk.model_exchange_id, received_at)
            .is_ok();
        let observed_usage = primary_model_usage(chunk).ok().flatten();
        let usage = observed_usage.unwrap_or_default();
        let _ = self.ordinal_store.record_performance_completion(
            &owner.run_key,
            PerformanceOperationKind::PrimaryModel,
            &owner.model_call_id,
            received_at,
            PerformanceOperationCompletion {
                usage_known: observed_usage.is_some(),
                cache_breakdown_known: observed_usage.is_some() && usage.cached.is_some(),
                duration_millis: None,
                input_tokens: usage.input,
                cached_tokens: usage.cached.unwrap_or(0),
                output_tokens: usage.output,
                actual_cost_microunits: usage.actual_cost_microunits,
            },
        );
        jev_complete
    }

    fn retain_device_jev_receipts(
        &self,
        owner: &ModelExchangeOwner,
        exchange: &ModelExchangeId,
        received_at: &Instant,
    ) -> Result<(), BridgeError> {
        if let Some(directory) = self.device_provider_directory.get() {
            let open = self
                .outbox
                .retained_model_open(exchange)
                .map_err(|_| BridgeError::Unavailable)?
                .ok_or(BridgeError::Conflict)?;
            let device = winwincode_provider::DeviceProviderStore::open(directory)
                .map_err(|_| BridgeError::Unavailable)?;
            for receipt in device
                .model_jev_receipts(&open)
                .map_err(|_| BridgeError::Unavailable)?
            {
                self.ordinal_store
                    .retain_jev_performance(&owner.run_key, &receipt, received_at)
                    .map_err(model_frame_store_error)?;
            }
            for receipt in device
                .model_jev_judge_receipts(&open)
                .map_err(|_| BridgeError::Unavailable)?
            {
                self.ordinal_store
                    .retain_jev_performance(&owner.run_key, &receipt, received_at)
                    .map_err(model_frame_store_error)?;
            }
        }
        Ok(())
    }

    fn validate_chunk_authority(
        &self,
        chunk: &ModelChunkMessage,
        binding: &ModelRunBinding,
        received_at: &Instant,
    ) -> Result<(), BridgeError> {
        if !canonical_instant(received_at)
            || received_at.0 < binding.authority.lease.issued_at.0
            || chunk.worker_session_id != binding.authority.worker_session_id
            || chunk.session_identity != binding.authority.session_identity
        {
            return Err(BridgeError::StaleAuthority);
        }
        let authority = ModelLeaseAuthority {
            lease: chunk.lease.clone(),
            worker_session_id: chunk.worker_session_id.clone(),
            session_identity: chunk.session_identity.clone(),
        };
        self.authority
            .validate_retained_exchange(&authority, &chunk.model_exchange_id, received_at)
            .map_err(|_| BridgeError::StaleAuthority)
    }

    fn resolve_chunk_owner(
        &self,
        chunk: &ModelChunkMessage,
    ) -> Result<(ModelExchangeOwner, ModelRunBinding, bool), BridgeError> {
        let process_owner = self
            .exchanges
            .lock()
            .map_err(|_| BridgeError::Unavailable)?
            .get(&chunk.model_exchange_id.0)
            .cloned();
        let process_exchange = process_owner.is_some();
        let thread_id = process_owner.as_ref().map_or_else(
            || chunk.session_identity.codex_thread_id.clone(),
            |owner| owner.thread_id.clone(),
        );
        if thread_id != chunk.session_identity.codex_thread_id {
            return Err(BridgeError::StaleAuthority);
        }
        let binding = if let Some(binding) = self.binding_for_thread(&thread_id)? {
            binding
        } else {
            self.durable_binding_for_thread(&thread_id)?
                .ok_or(BridgeError::StaleAuthority)?
        };
        if chunk.worker_session_id != binding.authority.worker_session_id
            || chunk.session_identity != binding.authority.session_identity
        {
            return Err(BridgeError::StaleAuthority);
        }
        let owner = if let Some(owner) = process_owner {
            if owner.run_key != binding.run_key {
                return Err(BridgeError::StaleAuthority);
            }
            owner
        } else {
            let Some(model_call_id) = self
                .ordinal_store
                .model_call_for_exchange(&binding.run_key, &chunk.model_exchange_id)
                .map_err(model_frame_store_error)?
            else {
                return Err(BridgeError::UnknownExchange);
            };
            ModelExchangeOwner {
                thread_id,
                run_key: binding.run_key.clone(),
                model_call_id,
            }
        };
        Ok((owner, binding, process_exchange))
    }

    fn durable_binding_for_thread(
        &self,
        thread_id: &CodexThreadId,
    ) -> Result<Option<ModelRunBinding>, BridgeError> {
        let Some((run_key, authority)) = self.authority.load_lineage_binding(&thread_id.0)? else {
            return Ok(None);
        };
        if authority.session_identity.codex_thread_id != *thread_id {
            return Err(BridgeError::StaleAuthority);
        }
        Ok(Some(ModelRunBinding {
            run_key,
            canonical_thread_id: thread_id.clone(),
            kernel_session_id: thread_id.0.clone(),
            opened_at: authority.lease.issued_at.clone(),
            authority,
        }))
    }

    pub(crate) async fn cancel_thread(
        &self,
        thread_id: &CodexThreadId,
        cancelled_at: &Instant,
    ) -> Result<(), BridgeError> {
        let run_keys = self
            .bindings
            .read()
            .map_err(|_| BridgeError::Unavailable)?
            .values()
            .filter(|binding| binding.canonical_thread_id == *thread_id)
            .map(|binding| binding.run_key.clone())
            .collect::<std::collections::HashSet<_>>();
        let exchanges = self
            .exchanges
            .lock()
            .map_err(|_| BridgeError::Unavailable)?
            .iter()
            .filter_map(|(exchange, owner)| {
                (owner.thread_id == *thread_id || run_keys.contains(&owner.run_key))
                    .then_some((exchange.clone(), owner.clone()))
            })
            .collect::<Vec<_>>();
        for (exchange, owner) in exchanges {
            let exchange = ModelExchangeId(exchange);
            self.client
                .lock()
                .await
                .cancel_exchange(
                    &exchange,
                    ModelMessageMetadata {
                        message_id: execution_message_id(b"model-cancel", &exchange.0),
                        sent_at: cancelled_at.clone(),
                    },
                )
                .await
                .map_err(|_| BridgeError::Protocol)?;
            let jev_complete = self
                .retain_device_jev_receipts(&owner, &exchange, cancelled_at)
                .is_ok();
            let _ = self.ordinal_store.close_unmeasured_primary(
                &owner.run_key,
                &owner.model_call_id,
                cancelled_at,
            );
            self.mark_provider_final(&owner, jev_complete)?;
            self.client
                .lock()
                .await
                .release_terminal(&exchange)
                .await
                .map_err(|_| BridgeError::Protocol)?;
            self.exchanges
                .lock()
                .map_err(|_| BridgeError::Unavailable)?
                .remove(&exchange.0);
        }
        Ok(())
    }

    pub(crate) fn take_messages(&self) -> Result<Vec<ExecutionPortMessage>, BridgeError> {
        let messages = self.queue.take()?;
        Ok(messages)
    }

    pub(crate) fn restore_messages(
        &self,
        messages: Vec<ExecutionPortMessage>,
    ) -> Result<(), BridgeError> {
        let mut queue = self.queue.lock()?;
        for message in messages.into_iter().rev() {
            queue.push_front(message);
        }
        Ok(())
    }

    pub(crate) fn discard_messages_for_thread(
        &self,
        thread_id: &CodexThreadId,
    ) -> Result<(), BridgeError> {
        self.queue.discard_for_thread(thread_id)
    }

    pub(crate) fn binding_for_thread(
        &self,
        thread_id: &CodexThreadId,
    ) -> Result<Option<ModelRunBinding>, BridgeError> {
        Ok(self
            .bindings
            .read()
            .map_err(|_| BridgeError::Unavailable)?
            .get(&thread_id.0)
            .cloned())
    }

    fn binding_for(
        &self,
        envelope: &KernelRequestEnvelope,
    ) -> Result<ModelRunBinding, BridgeError> {
        let binding = {
            let bindings = self.bindings.read().map_err(|_| BridgeError::Unavailable)?;
            bindings
                .get(&envelope.thread_id)
                .or_else(|| bindings.get(&envelope.session_id))
                .cloned()
        };
        if let Some(binding) = binding
            && envelope.session_id == binding.kernel_session_id
            && (envelope.thread_id == binding.kernel_session_id
                || envelope.thread_id == binding.canonical_thread_id.0)
        {
            return Ok(binding);
        }
        if let Some((run_key, authority)) =
            self.authority.load_lineage_binding(&envelope.thread_id)?
            && authority.session_identity.codex_thread_id.0 == envelope.thread_id
        {
            if envelope.session_id.trim().is_empty() {
                return Err(BridgeError::InvalidPayload);
            }
            let binding = ModelRunBinding {
                run_key,
                canonical_thread_id: CodexThreadId(envelope.thread_id.clone()),
                kernel_session_id: envelope.session_id.clone(),
                opened_at: authority.lease.issued_at.clone(),
                authority,
            };
            self.install_child_binding(binding.clone())?;
            return Ok(binding);
        }
        self.derive_child_binding(envelope)
    }

    fn derive_child_binding(
        &self,
        envelope: &KernelRequestEnvelope,
    ) -> Result<ModelRunBinding, BridgeError> {
        let parent_thread_id =
            parent_thread_id(&envelope.request).ok_or(BridgeError::StaleAuthority)?;
        let parent = if let Some(parent) = self
            .bindings
            .read()
            .map_err(|_| BridgeError::Unavailable)?
            .get(&parent_thread_id)
            .cloned()
        {
            parent
        } else {
            let Some((run_key, authority)) =
                self.authority.load_lineage_binding(&parent_thread_id)?
            else {
                return Err(BridgeError::StaleAuthority);
            };
            ModelRunBinding {
                run_key,
                canonical_thread_id: authority.session_identity.codex_thread_id.clone(),
                kernel_session_id: parent_thread_id.clone(),
                opened_at: authority.lease.issued_at.clone(),
                authority,
            }
        };
        if envelope.thread_id.trim().is_empty() || envelope.session_id.trim().is_empty() {
            return Err(BridgeError::InvalidPayload);
        }
        let mut session_identity = parent.authority.session_identity.clone();
        session_identity.codex_thread_id = CodexThreadId(envelope.thread_id.clone());
        let binding = ModelRunBinding {
            run_key: parent.run_key,
            canonical_thread_id: CodexThreadId(envelope.thread_id.clone()),
            kernel_session_id: envelope.session_id.clone(),
            authority: ModelLeaseAuthority {
                lease: parent.authority.lease,
                worker_session_id: parent.authority.worker_session_id,
                session_identity,
            },
            opened_at: parent.opened_at,
        };
        self.install_child_binding(binding.clone())?;
        Ok(binding)
    }

    async fn open_stream(
        self: Arc<Self>,
        request: ModelPortRequest,
        fusion_member: Option<String>,
    ) -> Result<ModelPortStream, ModelPortFailure> {
        let prepared = self.prepare_member_stream(&request, fusion_member.as_deref())?;
        let exchange = match prepared {
            OpenStreamPreparation::Replay(stream) => return Ok(stream),
            OpenStreamPreparation::Open(exchange) => *exchange,
        };
        let command = self.open_command(&exchange)?;
        let receiver = self
            .sink
            .register(&exchange.exchange)
            .map_err(model_failure)?;
        self.exchanges
            .lock()
            .map_err(|_| model_failure(BridgeError::Unavailable))?
            .insert(
                exchange.exchange.0.clone(),
                ModelExchangeOwner {
                    thread_id: exchange.binding.canonical_thread_id.clone(),
                    run_key: exchange.binding.run_key.clone(),
                    model_call_id: exchange.model_call_id.clone(),
                },
            );
        let opened = self.client.lock().await.open(command).await;
        if let Err(error) = opened {
            let closed_at = self
                .authority
                .observed_now()
                .map_err(model_failure)?
                .unwrap_or_else(|| exchange.binding.opened_at.clone());
            let close = self.ordinal_store.close_unmeasured_primary(
                &exchange.binding.run_key,
                &exchange.model_call_id,
                &closed_at,
            );
            let _ = self.sink.remove(&exchange.exchange);
            if let Ok(mut exchanges) = self.exchanges.lock() {
                exchanges.remove(&exchange.exchange.0);
            }
            close.map_err(model_store_failure)?;
            return Err(match error.code() {
                ModelPortClientErrorCode::ExpiredLease => ModelPortFailure::new(
                    "MODEL_AUTHORITY_EXPIRED",
                    "model call authority has expired",
                ),
                ModelPortClientErrorCode::StaleAuthority => {
                    model_failure(BridgeError::StaleAuthority)
                }
                ModelPortClientErrorCode::InvalidInput
                | ModelPortClientErrorCode::CanonicalMapping => {
                    model_failure(BridgeError::InvalidPayload)
                }
                ModelPortClientErrorCode::UnknownExchange => {
                    model_failure(BridgeError::UnknownExchange)
                }
                ModelPortClientErrorCode::AlreadyTerminal
                | ModelPortClientErrorCode::ExchangeConflict
                | ModelPortClientErrorCode::CursorCorrupt => model_failure(BridgeError::Conflict),
                ModelPortClientErrorCode::CursorStore
                | ModelPortClientErrorCode::CodexSink
                | ModelPortClientErrorCode::ExecutionPort => {
                    model_failure(BridgeError::Unavailable)
                }
            });
        }
        let stream = stream::unfold(receiver, |mut receiver| async move {
            receiver.recv().await.map(|item| (item, receiver))
        });
        Ok(Box::pin(stream))
    }

    #[cfg(test)]
    fn prepare_open_stream(
        &self,
        request: &ModelPortRequest,
    ) -> Result<OpenStreamPreparation, ModelPortFailure> {
        self.prepare_member_stream(request, None)
    }

    #[cfg(test)]
    pub(crate) fn prepare_open_stream_for_storage_measurement(
        &self,
        request: &ModelPortRequest,
    ) -> Result<(), ModelPortFailure> {
        self.prepare_member_stream(request, None).map(drop)
    }

    pub(crate) fn prepare_host_payload(
        &self,
        request: &ModelPortRequest,
        stored: Option<&serde_json::Value>,
        run_key: &str,
        is_fusion_member: bool,
    ) -> Result<Vec<u8>, ModelPortFailure> {
        let mut payload_bytes = if is_fusion_member {
            request.payload_json.as_bytes().to_vec()
        } else {
            sealed_context_payload(request, stored)?
        };
        let mut payload: serde_json::Value = serde_json::from_slice(&payload_bytes)
            .map_err(|_| model_failure(BridgeError::InvalidPayload))?;
        if payload
            .pointer("/winwincodeJevContext/profile/source/settings/jevJudge")
            .is_some()
        {
            let job = stored
                .as_ref()
                .and_then(|run| run.get("job"))
                .ok_or_else(|| model_failure(BridgeError::InvalidPayload))?;
            payload["winwincodeJevJudge"] = serde_json::to_value(
                crate::judge_bridge::requests(&self.ordinal_store, run_key, &payload, job)
                    .map_err(model_store_failure)?,
            )
            .map_err(|_| model_failure(BridgeError::InvalidPayload))?;
            payload_bytes = serde_json::to_vec(&payload)
                .map_err(|_| model_failure(BridgeError::InvalidPayload))?;
        }
        if !is_fusion_member
            && let Some(run) = stored
            && let Some(context) = crate::task_handoff::context_snapshot(run)
                .map_err(|_| model_failure(BridgeError::InvalidPayload))?
        {
            let context = self
                .ordinal_store
                .retain_model_task_context(
                    run_key,
                    &request.request_id,
                    &model_call_digest(&payload["request"]).map_err(model_failure)?,
                    context.as_bytes(),
                )
                .map_err(model_store_failure)?;
            let context = std::str::from_utf8(&context)
                .map_err(|_| model_failure(BridgeError::InvalidPayload))?;
            crate::task_handoff::attach_context(&mut payload, context)
                .map_err(|_| model_failure(BridgeError::InvalidPayload))?;
            payload_bytes = serde_json::to_vec(&payload)
                .map_err(|_| model_failure(BridgeError::InvalidPayload))?;
        }
        Ok(payload_bytes)
    }

    fn prepare_member_stream(
        &self,
        request: &ModelPortRequest,
        fusion_member: Option<&str>,
    ) -> Result<OpenStreamPreparation, ModelPortFailure> {
        let envelope: KernelRequestEnvelope = serde_json::from_str(&request.payload_json)
            .map_err(|_| model_failure(BridgeError::InvalidPayload))?;
        if envelope.request_id != request.request_id || !envelope.request.is_object() {
            return Err(model_failure(BridgeError::InvalidPayload));
        }
        let binding = self.binding_for(&envelope).map_err(model_failure)?;
        let stored = self
            .ordinal_store
            .load_run_header::<serde_json::Value>(&binding.run_key)
            .map_err(model_store_failure)?;
        if let Some(member) = fusion_member {
            validate_fusion_model_selection(&envelope, stored.as_ref(), member)?;
        } else {
            validate_model_selection(&envelope, stored.as_ref(), &self.expected_provider)?;
        }
        self.sync_action_binding(&binding).map_err(model_failure)?;
        let payload_bytes = self.prepare_host_payload(
            request,
            stored.as_ref(),
            &binding.run_key,
            fusion_member.is_some(),
        )?;
        let request_digest = model_call_digest(&if let Some(member) = fusion_member {
            serde_json::json!({"fusionMember":member,"provider":envelope.provider,"request":envelope.request})
        } else { envelope.request.clone() }).map_err(model_failure)?;
        let model_call_id = envelope.request_id.clone();
        let exchange_identity = format!("{}:{model_call_id}", binding.run_key);
        let exchange = ModelExchangeId(canonical_id(
            "mdl",
            b"worker-model-exchange",
            &exchange_identity,
        ));
        let _ordinal = self
            .ordinal_store
            .claim_model_call(&binding.run_key, &model_call_id, &exchange, &request_digest)
            .map_err(model_store_failure)?;
        let observed_at = self
            .authority
            .observed_now()
            .map_err(model_failure)?
            .unwrap_or_else(|| binding.opened_at.clone());
        let _ = self.ordinal_store.record_performance_start(
            &binding.run_key,
            PerformanceOperationKind::PrimaryModel,
            &model_call_id,
            &observed_at,
        );
        let phase = self
            .ordinal_store
            .model_call_phase(&binding.run_key, &model_call_id)
            .map_err(model_store_failure)?;
        let frames = self.load_model_call_frames(&binding.run_key, &model_call_id)?;
        if matches!(
            phase,
            Some(ModelCallPhase::ProviderFinal | ModelCallPhase::CoreCommitted)
        ) || frames.last().is_some_and(|frame| frame.is_final)
        {
            if phase == Some(ModelCallPhase::InFlight) {
                self.ordinal_store
                    .mark_model_call_provider_final(&binding.run_key, &model_call_id)
                    .map_err(model_store_failure)?;
            }
            return Ok(OpenStreamPreparation::Replay(replay_model_call_frames(
                &frames,
                &binding,
                &exchange,
                &self.authority,
            )?));
        }
        let prepared_payload: serde_json::Value = serde_json::from_slice(&payload_bytes)
            .map_err(|_| model_failure(BridgeError::InvalidPayload))?;
        let prepared_request_digest =
            model_open_request_digest(&prepared_payload).map_err(model_failure)?;
        Ok(OpenStreamPreparation::Open(Box::new(OpenStreamExchange {
            binding,
            payload_bytes,
            request_digest: prepared_request_digest,
            model_call_id,
            exchange_identity,
            exchange,
        })))
    }

    fn load_model_call_frames(
        &self,
        run_key: &str,
        model_call_id: &str,
    ) -> Result<Vec<ModelChunkMessage>, ModelPortFailure> {
        self.ordinal_store
            .load_model_call_frames(run_key, model_call_id)
            .map_err(model_store_failure)
    }

    fn open_command(
        &self,
        exchange: &OpenStreamExchange,
    ) -> Result<OpenModelExchangeCommand, ModelPortFailure> {
        let command = match self
            .outbox
            .retained_model_open(&exchange.exchange)
            .map_err(|_| model_failure(BridgeError::Unavailable))?
        {
            Some(open) => {
                let original_request = decode_payload(&open.request).map_err(model_failure)?;
                let original_digest =
                    model_open_request_digest(&original_request).map_err(model_failure)?;
                let original_authority = ModelLeaseAuthority {
                    lease: open.lease.clone(),
                    worker_session_id: open.worker_session_id.clone(),
                    session_identity: open.session_identity.clone(),
                };
                if !same_binding_with_extended_lease(
                    &original_authority,
                    &exchange.binding.authority,
                ) || open.worker_session_id != exchange.binding.authority.worker_session_id
                    || open.session_identity != exchange.binding.authority.session_identity
                    || open.route != self.route
                    || original_digest != exchange.request_digest
                {
                    return Err(model_failure(BridgeError::Conflict));
                }
                OpenModelExchangeCommand {
                    metadata: ModelMessageMetadata {
                        message_id: open.message_id,
                        sent_at: open.sent_at,
                    },
                    authority: original_authority,
                    model_exchange_id: exchange.exchange.clone(),
                    request_id: open.request_id,
                    route: open.route,
                    request: open.request,
                }
            }
            None => OpenModelExchangeCommand {
                metadata: ModelMessageMetadata {
                    message_id: execution_message_id(b"model-open", &exchange.exchange_identity),
                    sent_at: exchange.binding.opened_at.clone(),
                },
                authority: exchange.binding.authority.clone(),
                model_exchange_id: exchange.exchange.clone(),
                request_id: RequestId(canonical_id(
                    "req",
                    b"worker-model-request",
                    &exchange.exchange_identity,
                )),
                route: self.route.clone(),
                request: encoded(&exchange.payload_bytes),
            },
        };
        Ok(command)
    }
}

impl fmt::Debug for ExecutionPortModelBridge {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ExecutionPortModelBridge")
            .field("route", &self.route)
            .finish_non_exhaustive()
    }
}

#[derive(Debug)]
struct KernelModelPort {
    bridge: Arc<ExecutionPortModelBridge>,
    fusion_member: Option<String>,
}

impl ModelPort for KernelModelPort {
    fn compaction_context_overhead_bytes(
        &self,
        thread_id: &str,
    ) -> Result<usize, ModelPortFailure> {
        if self.fusion_member.is_some() {
            return Ok(0);
        }
        let thread = CodexThreadId(thread_id.to_owned());
        let binding = match self
            .bridge
            .binding_for_thread(&thread)
            .map_err(model_failure)?
        {
            Some(binding) => binding,
            None => self
                .bridge
                .durable_binding_for_thread(&thread)
                .map_err(model_failure)?
                .ok_or_else(|| model_failure(BridgeError::InvalidPayload))?,
        };
        self.bridge.task_context_overhead_bytes(&binding.run_key)
    }

    fn stream(
        &self,
        request: ModelPortRequest,
    ) -> BoxFuture<'static, Result<ModelPortStream, ModelPortFailure>> {
        let bridge = Arc::clone(&self.bridge);
        let member = self.fusion_member.clone();
        Box::pin(async move { bridge.open_stream(request, member).await })
    }
}

pub(crate) fn sealed_context_payload(
    request: &ModelPortRequest,
    stored: Option<&serde_json::Value>,
) -> Result<Vec<u8>, ModelPortFailure> {
    use winwincode_execution_port::agent_config::{
        AgentSessionConfigSnapshot, validate_agent_session_config,
    };
    let Some(run) = stored else {
        return Ok(request.payload_json.as_bytes().to_vec());
    };
    let config = run
        .get("agentConfig")
        .ok_or_else(|| model_failure(BridgeError::InvalidPayload))?;
    let snapshot: AgentSessionConfigSnapshot = serde_json::from_value(config.clone())
        .map_err(|_| model_failure(BridgeError::InvalidPayload))?;
    validate_agent_session_config(&snapshot)
        .map_err(|_| model_failure(BridgeError::InvalidPayload))?;
    if snapshot.profile.source.settings.jev_context.is_none()
        && snapshot.profile.source.settings.jev_judge.is_none()
    {
        return Ok(request.payload_json.as_bytes().to_vec());
    }
    let job_value = run
        .get("job")
        .ok_or_else(|| model_failure(BridgeError::InvalidPayload))?;
    let job: winwincode_execution_port::generated::ExecutionJob =
        serde_json::from_value(job_value.clone())
            .map_err(|_| model_failure(BridgeError::InvalidPayload))?;
    let digest = crate::stage_product::stage_product_job_digest(&job)
        .map_err(|_| model_failure(BridgeError::InvalidPayload))?;
    if run.get("jobDigest").and_then(serde_json::Value::as_str) != Some(digest.0.as_str()) {
        return Err(model_failure(BridgeError::InvalidPayload));
    }
    let mut payload: serde_json::Value = serde_json::from_str(&request.payload_json)
        .map_err(|_| model_failure(BridgeError::InvalidPayload))?;
    payload["winwincodeJevContext"] = config.clone();
    payload["winwincodeJevTask"] = serde_json::json!({"job": job, "jobDigest": digest});
    serde_json::to_vec(&payload).map_err(|_| model_failure(BridgeError::InvalidPayload))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct KernelRequestEnvelope {
    request_id: String,
    provider: String,
    session_id: String,
    thread_id: String,
    #[serde(default, rename = "turnId")]
    _turn_id: Option<String>,
    request: serde_json::Value,
}

fn validate_fusion_model_selection(
    envelope: &KernelRequestEnvelope,
    stored: Option<&serde_json::Value>,
    member_id: &str,
) -> Result<(), ModelPortFailure> {
    use winwincode_execution_port::agent_config::{
        AgentSessionConfigSnapshot, validate_agent_session_config,
    };
    let invalid = || model_failure(BridgeError::InvalidPayload);
    let snapshot: AgentSessionConfigSnapshot = serde_json::from_value(
        stored
            .and_then(|run| run.get("agentConfig"))
            .cloned()
            .ok_or_else(invalid)?,
    )
    .map_err(|_| invalid())?;
    validate_agent_session_config(&snapshot).map_err(|_| invalid())?;
    let fusion = snapshot
        .profile
        .source
        .settings
        .fusion
        .ok_or_else(invalid)?;
    let member = fusion
        .members
        .iter()
        .find(|member| member.id == member_id)
        .ok_or_else(invalid)?;
    if envelope.provider != member.provider
        || envelope.request["model"].as_str() != Some(&member.model)
        || envelope
            .request
            .pointer("/reasoning/effort")
            .and_then(serde_json::Value::as_str)
            != Some(&member.reasoning)
    {
        return Err(invalid());
    }
    Ok(())
}

fn validate_model_selection(
    envelope: &KernelRequestEnvelope,
    stored: Option<&serde_json::Value>,
    expected_provider: &str,
) -> Result<(), ModelPortFailure> {
    let provider = match stored {
        Some(run) => run
            .pointer("/agentConfig/profile/source/settings/provider")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| model_failure(BridgeError::InvalidPayload))?,
        None => expected_provider,
    };
    if envelope.provider != provider {
        return Err(model_failure(BridgeError::InvalidPayload));
    }
    if let Some(run) = stored {
        let settings = run
            .pointer("/agentConfig/profile/source/settings")
            .ok_or_else(|| model_failure(BridgeError::InvalidPayload))?;
        let model = settings["model"]
            .as_str()
            .ok_or_else(|| model_failure(BridgeError::InvalidPayload))?;
        let reasoning = settings["reasoning"]
            .as_str()
            .ok_or_else(|| model_failure(BridgeError::InvalidPayload))?;
        if envelope.request["model"].as_str() != Some(model)
            || (reasoning != "provider_default"
                && envelope
                    .request
                    .pointer("/reasoning/effort")
                    .and_then(serde_json::Value::as_str)
                    != Some(reasoning))
        {
            return Err(ModelPortFailure::new(
                "INVALID_REQUEST",
                "model request differs from sealed session settings",
            ));
        }
    }
    Ok(())
}

fn model_call_digest(
    request: &serde_json::Value,
) -> Result<winwincode_domain::Sha256Digest, BridgeError> {
    let mut canonical = request.clone();
    canonical
        .as_object_mut()
        .ok_or(BridgeError::InvalidPayload)?
        .remove("client_metadata");
    let bytes = serde_json::to_vec(&canonical).map_err(|_| BridgeError::InvalidPayload)?;
    Ok(winwincode_domain::Sha256Digest(format!(
        "sha256:{:x}",
        Sha256::digest(bytes)
    )))
}

fn model_open_request_digest(
    payload: &serde_json::Value,
) -> Result<winwincode_domain::Sha256Digest, BridgeError> {
    let request = payload
        .get("request")
        .filter(|request| request.is_object())
        .ok_or(BridgeError::InvalidPayload)?;
    model_call_digest(request)
}

pub(crate) fn extend_binding_lineage(
    bindings: &mut HashMap<String, ModelRunBinding>,
    binding: &ModelRunBinding,
) -> bool {
    let Some(previous) = bindings.get(&binding.canonical_thread_id.0).cloned() else {
        return false;
    };
    let mut extended = previous.clone();
    extended.authority.lease.expires_at = binding.authority.lease.expires_at.clone();
    if !same_binding(&extended, binding)
        || !same_binding_with_extended_lease(&previous.authority, &binding.authority)
    {
        return false;
    }
    for existing in bindings.values_mut() {
        if existing.run_key == previous.run_key
            && common_authority(&existing.authority, &previous.authority)
        {
            existing.authority.lease.expires_at = binding.authority.lease.expires_at.clone();
        }
    }
    bindings.insert(binding.kernel_session_id.clone(), binding.clone());
    bindings.insert(binding.canonical_thread_id.0.clone(), binding.clone());
    true
}

fn same_binding(left: &ModelRunBinding, right: &ModelRunBinding) -> bool {
    left.run_key == right.run_key
        && left.canonical_thread_id == right.canonical_thread_id
        && left.kernel_session_id == right.kernel_session_id
        && left.authority == right.authority
}

pub(crate) fn same_binding_with_extended_lease(
    previous: &ModelLeaseAuthority,
    current: &ModelLeaseAuthority,
) -> bool {
    if current.lease.expires_at.0 < previous.lease.expires_at.0 {
        return false;
    }
    let mut extended = previous.clone();
    extended.lease.expires_at = current.lease.expires_at.clone();
    extended == *current
}

pub(crate) fn common_authority(left: &ModelLeaseAuthority, right: &ModelLeaseAuthority) -> bool {
    left.lease == right.lease
        && left.worker_session_id == right.worker_session_id
        && left.session_identity.product_session_id == right.session_identity.product_session_id
        && left.session_identity.work_run_id == right.session_identity.work_run_id
        && left.session_identity.worker_session_id == right.session_identity.worker_session_id
}

fn parent_thread_id(request: &serde_json::Value) -> Option<String> {
    let metadata = request.get("client_metadata")?.as_object()?;
    let direct = metadata
        .get("x-codex-parent-thread-id")
        .or_else(|| metadata.get("parent_thread_id"))
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(ToOwned::to_owned);
    if direct.is_some() {
        return direct;
    }
    let encoded = metadata
        .get("x-codex-turn-metadata")
        .and_then(serde_json::Value::as_str)?;
    let value: serde_json::Value = serde_json::from_str(encoded).ok()?;
    value
        .get("parent_thread_id")
        .or_else(|| value.get("parentThreadId"))
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(ToOwned::to_owned)
}

fn decode_payload(payload: &EncodedPayload) -> Result<serde_json::Value, BridgeError> {
    if payload.content_type != "application/json" {
        return Err(BridgeError::InvalidPayload);
    }
    let bytes = STANDARD
        .decode(&payload.data_base64)
        .map_err(|_| BridgeError::InvalidPayload)?;
    if format!("sha256:{:x}", Sha256::digest(&bytes)) != payload.payload_digest.0 {
        return Err(BridgeError::InvalidPayload);
    }
    let request: serde_json::Value =
        serde_json::from_slice(&bytes).map_err(|_| BridgeError::InvalidPayload)?;
    if !request.is_object() {
        return Err(BridgeError::InvalidPayload);
    }
    Ok(request)
}

fn primary_model_usage(
    chunk: &ModelChunkMessage,
) -> Result<Option<PrimaryModelUsage>, BridgeError> {
    let Some(payload) = &chunk.payload else {
        return Ok(None);
    };
    let payload = decode_payload(payload)?;
    let usage = payload
        .get("tokenUsage")
        .or_else(|| payload.get("token_usage"));
    let Some(usage) = usage else {
        return Ok(None);
    };
    let input = non_negative_metric(usage, "inputTokens", "input_tokens")?;
    let cached = usage
        .get("cachedInputTokens")
        .or_else(|| usage.get("cached_input_tokens"))
        .filter(|value| !value.is_null())
        .map(|_| non_negative_metric(usage, "cachedInputTokens", "cached_input_tokens"))
        .transpose()?;
    let output = non_negative_metric(usage, "outputTokens", "output_tokens")?;
    let actual_cost_microunits = payload
        .get("actualCostMicros")
        .or_else(|| payload.get("actual_cost_micros"))
        .filter(|value| !value.is_null())
        .map(|_| non_negative_metric(&payload, "actualCostMicros", "actual_cost_micros"))
        .transpose()?;
    if cached.is_some_and(|cached| cached > input) {
        return Err(BridgeError::InvalidPayload);
    }
    Ok(Some(PrimaryModelUsage {
        input: input.saturating_sub(cached.unwrap_or(0)),
        cached,
        output,
        actual_cost_microunits,
    }))
}

fn non_negative_metric(
    value: &serde_json::Value,
    camel_case: &str,
    snake_case: &str,
) -> Result<i64, BridgeError> {
    value
        .get(camel_case)
        .or_else(|| value.get(snake_case))
        .and_then(serde_json::Value::as_i64)
        .filter(|value| *value >= 0)
        .ok_or(BridgeError::InvalidPayload)
}

fn replay_model_call_frames(
    frames: &[ModelChunkMessage],
    binding: &ModelRunBinding,
    exchange: &ModelExchangeId,
    authority: &SharedAuthoritySource,
) -> Result<ModelPortStream, ModelPortFailure> {
    if frames.is_empty() {
        return Err(model_failure(BridgeError::Unavailable));
    }
    let mut items = Vec::new();
    for (index, chunk) in frames.iter().enumerate() {
        let expected_sequence =
            i64::try_from(index + 1).map_err(|_| model_failure(BridgeError::InvalidPayload))?;
        if chunk.sequence.0 != expected_sequence
            || chunk.model_exchange_id != *exchange
            || chunk.worker_session_id != binding.authority.worker_session_id
            || chunk.session_identity != binding.authority.session_identity
            || (chunk.is_final && index + 1 != frames.len())
            || (chunk.error.is_some() && !chunk.is_final)
        {
            return Err(model_failure(BridgeError::Conflict));
        }
        authority
            .validate_retained_exchange(
                &ModelLeaseAuthority {
                    lease: chunk.lease.clone(),
                    worker_session_id: chunk.worker_session_id.clone(),
                    session_identity: chunk.session_identity.clone(),
                },
                exchange,
                &binding.opened_at,
            )
            .map_err(|_| model_failure(BridgeError::StaleAuthority))?;
        if let Some(error) = &chunk.error {
            items
                .push(Err(decode_provider_failure(error, chunk.payload.as_ref())
                    .map_err(model_failure)?));
        } else if let Some(payload) = &chunk.payload {
            items.push(Ok(decode_response_payload(payload)?));
        } else if !chunk.is_final {
            return Err(model_failure(BridgeError::InvalidPayload));
        }
    }
    if !frames.last().is_some_and(|frame| frame.is_final) {
        return Err(model_failure(BridgeError::Conflict));
    }
    Ok(Box::pin(stream::iter(items)) as ModelPortStream)
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DeviceModelFailureMetadata {
    provider_error_kind: Option<String>,
    status: Option<u16>,
    provider_retry_after_millis: Option<u64>,
    provider_request_id: Option<String>,
    diagnostic: Option<DeviceModelFailureDiagnostic>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DeviceModelFailureDiagnostic {
    stage: Option<String>,
    event_type: Option<String>,
    field_path: Option<String>,
}

fn decode_provider_failure(
    error: &ExecutionPortError,
    payload: Option<&EncodedPayload>,
) -> Result<ModelPortFailure, BridgeError> {
    let metadata: DeviceModelFailureMetadata = payload
        .map(|payload| {
            let json = decode_response_payload(payload).map_err(|_| BridgeError::InvalidPayload)?;
            serde_json::from_str(&json).map_err(|_| BridgeError::InvalidPayload)
        })
        .transpose()?
        .unwrap_or_default();
    if metadata
        .status
        .is_some_and(|status| !(100..=599).contains(&status))
        || metadata.provider_request_id.as_ref().is_some_and(|value| {
            value.is_empty()
                || value.len() > 256
                || !value.bytes().all(|byte| byte.is_ascii_graphic())
        })
        || metadata.provider_error_kind.as_ref().is_some_and(|value| {
            value.is_empty()
                || value.len() > 64
                || !value
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        })
        || metadata.diagnostic.as_ref().is_some_and(|diagnostic| {
            [
                &diagnostic.stage,
                &diagnostic.event_type,
                &diagnostic.field_path,
            ]
            .into_iter()
            .flatten()
            .any(|value| {
                value.is_empty() || value.len() > 256 || value.chars().any(char::is_control)
            })
        })
    {
        return Err(BridgeError::InvalidPayload);
    }
    let wire_code = serde_json::to_value(&error.code).map_err(|_| BridgeError::InvalidPayload)?;
    let wire_code = wire_code.as_str().ok_or(BridgeError::InvalidPayload)?;
    let code = match wire_code {
        "DEVICE_PROVIDER_RATE_LIMITED" => "RATE_LIMIT",
        "DEVICE_PROVIDER_UPSTREAM_FAILED" => "SERVER",
        "DEVICE_PROVIDER_CONNECTION_FAILED" | "DEVICE_PROVIDER_TRANSPORT_FAILED" => "TRANSPORT",
        "DEVICE_PROVIDER_RESPONSE_INCOMPLETE" => "INCOMPLETE_STREAM",
        "DEVICE_PROVIDER_REQUEST_REJECTED" => match metadata.status {
            Some(401 | 403) => "AUTH",
            Some(429) => "RATE_LIMIT",
            Some(500..=599) => "SERVER",
            _ => "INVALID_REQUEST",
        },
        "DEVICE_PROVIDER_REQUEST_INVALID"
        | "DEVICE_PROVIDER_REQUEST_TRANSLATION_FAILED"
        | "DEVICE_PROVIDER_REQUEST_TOO_LARGE"
        | "DEVICE_PROVIDER_INVALID_CONFIGURATION" => "INVALID_REQUEST",
        "DEVICE_PROVIDER_RESPONSE_CONTENT_TYPE_INVALID"
        | "DEVICE_PROVIDER_SSE_FRAMING_INVALID"
        | "DEVICE_PROVIDER_SSE_EVENT_INVALID"
        | "DEVICE_PROVIDER_STREAM_CONVERSION_FAILED"
        | "DEVICE_PROVIDER_ADAPTER_PROTOCOL_FAILED" => "PROTOCOL",
        _ => wire_code,
    };
    let terminal_authority = matches!(
        wire_code,
        "LEASE_EXPIRED"
            | "STALE_FENCING_TOKEN"
            | "WORKER_INSTANCE_CHANGED"
            | "DEVICE_MODEL_PAUSED"
            | "DEVICE_MODEL_INTERRUPTED"
            | "CANCELLED"
            | "DEVICE_MODEL_IDENTITY_CONFLICT"
            | "DEVICE_PROVIDER_CREDENTIAL_LEAK_BLOCKED"
    ) || matches!(code, "AUTH" | "INVALID_REQUEST");
    Ok(ModelPortFailure {
        code: code.to_owned(),
        message: "Provider Gateway returned a terminal model error".to_owned(),
        status: metadata.status,
        retryable: Some(error.retryable && !terminal_authority),
        provider_retry_after_millis: metadata.provider_retry_after_millis,
        provider_request_id: metadata.provider_request_id,
    })
}

fn decode_response_payload(payload: &EncodedPayload) -> Result<String, ModelPortFailure> {
    if payload.content_type != "application/json" {
        return Err(model_failure(BridgeError::InvalidPayload));
    }
    let bytes = STANDARD
        .decode(&payload.data_base64)
        .map_err(|_| model_failure(BridgeError::InvalidPayload))?;
    if format!("sha256:{:x}", Sha256::digest(&bytes)) != payload.payload_digest.0 {
        return Err(model_failure(BridgeError::InvalidPayload));
    }
    String::from_utf8(bytes).map_err(|_| model_failure(BridgeError::InvalidPayload))
}

fn model_frame_store_error(error: crate::store::AdapterStoreError) -> BridgeError {
    match error {
        crate::store::AdapterStoreError::Conflict => BridgeError::Conflict,
        crate::store::AdapterStoreError::Unavailable | crate::store::AdapterStoreError::Corrupt => {
            BridgeError::Unavailable
        }
    }
}

fn model_store_failure(error: crate::store::AdapterStoreError) -> ModelPortFailure {
    model_failure(model_frame_store_error(error))
}

static MODEL_INTAKE_LOG_PATH: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();

pub fn set_model_intake_log_path(path: impl Into<std::path::PathBuf>) {
    let path = path.into();
    let _ = MODEL_INTAKE_LOG_PATH.set(path);
}

fn model_intake_log_path() -> Option<std::path::PathBuf> {
    MODEL_INTAKE_LOG_PATH
        .get()
        .cloned()
        .or_else(|| std::env::var_os("WWC_MODEL_INTAKE_LOG").map(std::path::PathBuf::from))
}

pub(crate) fn log_model_intake_failure(chunk: &ModelChunkMessage, stage: &str, error: BridgeError) {
    use std::io::Write as _;

    let Some(path) = model_intake_log_path() else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    else {
        return;
    };
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default();
    let _ = writeln!(
        file,
        "ts={ts} component=model_bridge stage={stage} code={} exchange={} seq={} wsn={} thr={} run={} job={} wki={}",
        error.code(),
        chunk.model_exchange_id.0,
        chunk.sequence.0,
        chunk.worker_session_id.0,
        chunk.session_identity.codex_thread_id.0,
        chunk
            .session_identity
            .work_run_id
            .as_ref()
            .map_or("", |run| run.0.as_str()),
        chunk.lease.job_id.0,
        chunk.lease.worker_instance_id.0,
    );
}

pub(crate) fn validate_chunk_for_retention(chunk: &ModelChunkMessage) -> Result<(), BridgeError> {
    if chunk.sequence.0 <= 0 || (chunk.error.is_some() && !chunk.is_final) {
        return Err(BridgeError::InvalidPayload);
    }
    if let Some(payload) = &chunk.payload {
        if payload.content_type != "application/json" {
            return Err(BridgeError::InvalidPayload);
        }
        let bytes = STANDARD
            .decode(&payload.data_base64)
            .map_err(|_| BridgeError::InvalidPayload)?;
        if format!("sha256:{:x}", Sha256::digest(&bytes)) != payload.payload_digest.0 {
            return Err(BridgeError::InvalidPayload);
        }
        serde_json::from_slice::<serde_json::Value>(&bytes)
            .map_err(|_| BridgeError::InvalidPayload)?;
    } else if !chunk.is_final && chunk.error.is_none() {
        return Err(BridgeError::InvalidPayload);
    }
    if let Some(error) = &chunk.error {
        decode_provider_failure(error, chunk.payload.as_ref())?;
    }
    frame_from_message(&ExecutionPortMessage::ModelChunkMessage(chunk.clone()))
        .map_err(|_| BridgeError::InvalidPayload)?;
    Ok(())
}

fn encoded(bytes: &[u8]) -> EncodedPayload {
    EncodedPayload {
        content_type: "application/json".to_owned(),
        data_base64: STANDARD.encode(bytes),
        payload_digest: winwincode_domain::Sha256Digest(format!(
            "sha256:{:x}",
            Sha256::digest(bytes)
        )),
    }
}

fn execution_message_id(namespace: &[u8], input: &str) -> ExecutionMessageId {
    ExecutionMessageId(canonical_id("xmsg", namespace, input))
}

fn canonical_id(prefix: &str, namespace: &[u8], input: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(namespace);
    digest.update([0]);
    digest.update(input.as_bytes());
    let hex = format!("{:x}", digest.finalize());
    format!("{prefix}_{}", &hex[..26].to_ascii_uppercase())
}

fn model_failure(error: BridgeError) -> ModelPortFailure {
    ModelPortFailure::new(error.code(), error.to_string())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum BridgeError {
    Unavailable,
    Conflict,
    InvalidPayload,
    UnknownExchange,
    StaleAuthority,
    Protocol,
}

impl BridgeError {
    const fn code(self) -> &'static str {
        match self {
            Self::Unavailable => "MODEL_BRIDGE_UNAVAILABLE",
            Self::Conflict => "MODEL_BRIDGE_CONFLICT",
            Self::InvalidPayload => "MODEL_PAYLOAD_INVALID",
            Self::UnknownExchange => "MODEL_EXCHANGE_UNKNOWN",
            Self::StaleAuthority => "MODEL_AUTHORITY_STALE",
            Self::Protocol => "MODEL_PROTOCOL_REJECTED",
        }
    }
}

impl fmt::Display for BridgeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Unavailable => "embedded model bridge is unavailable",
            Self::Conflict => "embedded model exchange conflicts with current state",
            Self::InvalidPayload => "embedded model payload is invalid",
            Self::UnknownExchange => "embedded model exchange is unknown",
            Self::StaleAuthority => "embedded model authority is stale",
            Self::Protocol => "embedded model protocol frame was rejected",
        })
    }
}

impl std::error::Error for BridgeError {}

#[cfg(test)]
mod tests {
    include!("model_mechanism_regression_tests.rs");
    include!("model_local_cancellation_tests.rs");
    include!("model_task_context_replay_tests.rs");
    use serde_json::json;
    use std::sync::Arc;
    use winwincode_domain::{
        CodexThreadId, ExecutionJobId, FencingToken, Instant, LeaseId, ProductSessionId,
        SessionIdentity, WorkerId, WorkerInstanceId, WorkerSessionId,
    };
    use winwincode_execution_port::generated::ExecutionLeaseStamp;

    use super::{
        ExecutionPortModelBridge, ModelLeaseAuthority, ModelRunBinding, SharedAuthoritySource,
        model_call_digest, primary_model_usage,
    };
    use crate::model_port_client::{
        ModelAuthorityRejection, ModelChunkDisposition, ModelChunkFingerprint, ModelCursorStore,
        ModelLeaseAuthoritySource, ModelTerminationReason,
    };
    use crate::outbox::ExecutionOutbox;
    use crate::store::{AdapterStore, ModelCallPhase};
    use base64::Engine as _;
    use futures::StreamExt as _;
    use sha2::{Digest as _, Sha256};
    use winwincode_domain::{ExecutionMessageId, ExecutionSequence, ModelExchangeId};
    use winwincode_execution_port::generated::{
        EncodedPayload, ExecutionPortMessage, ModelChunkMessage, ModelChunkMessageKind,
        ModelGatewayRoute, ModelOpenMessage,
    };
    use winwincode_execution_port::runtime_trace_outbox::{ExecutionMode, ObserverMode};
    use winwincode_execution_port::typed_replay::frame_from_message;
    use winwincode_kernel::ModelPortRequest;

    #[tokio::test]
    #[allow(
        clippy::too_many_lines,
        reason = "Four real error envelopes through live delivery, durable replay and no extra Provider opens"
    )]
    async fn provider_failure_facts_match_live_and_retained_replay() {
        use winwincode_execution_port::generated::{ExecutionPortError, ExecutionPortErrorCode};

        let root =
            std::env::temp_dir().join(format!("wwc-provider-failure-{}", uuid::Uuid::now_v7()));
        let store = AdapterStore::open(&root).unwrap();
        let lease = authority(&id("cdx", 'A'));
        let bridge = Arc::new(installed_loopback_bridge(
            &store,
            SharedAuthoritySource::default(),
            "provider failure",
            "run",
            &lease,
            "kernel",
        ));
        for (index, (wire_code, status, canonical_code, retryable)) in [
            (
                ExecutionPortErrorCode::DeviceProviderRateLimited,
                429,
                "RATE_LIMIT",
                true,
            ),
            (
                ExecutionPortErrorCode::DeviceProviderUpstreamFailed,
                503,
                "SERVER",
                true,
            ),
            (
                ExecutionPortErrorCode::DeviceProviderRequestRejected,
                401,
                "AUTH",
                false,
            ),
            (
                ExecutionPortErrorCode::DeviceProviderRequestRejected,
                400,
                "INVALID_REQUEST",
                false,
            ),
        ]
        .into_iter()
        .enumerate()
        {
            let call = format!("failure-{index}");
            let request = ModelPortRequest {
                request_id: call.clone(),
                payload_json: json!({
                    "requestId":call, "provider":"loopback", "sessionId":"kernel",
                    "threadId":lease.session_identity.codex_thread_id.0,
                    "request":{"model":"loopback", "input":[]},
                })
                .to_string(),
            };
            let mut live = bridge.model_port().stream(request.clone()).await.unwrap();
            let open = bridge
                .take_messages()
                .unwrap()
                .into_iter()
                .find_map(|message| {
                    if let ExecutionPortMessage::ModelOpenMessage(open) = message {
                        Some(open)
                    } else {
                        None
                    }
                })
                .unwrap();
            let facts = json!({
                "providerErrorKind":serde_json::to_value(&wire_code).unwrap(), "status":status,
                "providerRetryAfterMillis":750, "providerRequestId":"upstream-42",
                "diagnostic":{"stage":"open","eventType":"error","fieldPath":"status"},
            })
            .to_string();
            let mut chunk = test_chunk(
                &lease,
                &open.model_exchange_id,
                1,
                true,
                facts.as_bytes(),
                'F',
            );
            chunk.error = Some(ExecutionPortError {
                code: wire_code,
                message: "private upstream response must not escape".to_owned(),
                retryable,
            });
            if index == 1 {
                bridge
                    .exchanges
                    .lock()
                    .unwrap()
                    .remove(&open.model_exchange_id.0);
            }
            bridge
                .accept_chunk(&chunk, &lease.lease.issued_at)
                .await
                .unwrap();
            let failure = live.next().await.unwrap().unwrap_err();
            assert_eq!(failure.code, canonical_code);
            assert_eq!(failure.status, Some(status));
            assert_eq!(failure.retryable, Some(retryable));
            assert_eq!(failure.provider_retry_after_millis, Some(750));
            assert_eq!(failure.provider_request_id.as_deref(), Some("upstream-42"));
            assert!(!failure.message.contains("private upstream response"));
            let mut replay = bridge.model_port().stream(request).await.unwrap();
            assert_eq!(replay.next().await.unwrap().unwrap_err(), failure);
            assert!(
                !bridge
                    .take_messages()
                    .unwrap()
                    .iter()
                    .any(|message| matches!(message, ExecutionPortMessage::ModelOpenMessage(_)))
            );
        }
        drop(bridge);
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn invalid_provider_failure_metadata_is_rejected_before_retention() {
        use winwincode_execution_port::generated::{ExecutionPortError, ExecutionPortErrorCode};

        let lease = authority(&id("cdx", 'A'));
        for facts in [
            json!({"status":"429"}),
            json!({"status":999}),
            json!({"providerRetryAfterMillis":-1}),
            json!({"providerRequestId":"request\nsecret"}),
            json!({"diagnostic":{"stage":12}}),
            json!({"tokenUsage":{"input_tokens":42}}),
        ] {
            let mut chunk = test_chunk(
                &lease,
                &ModelExchangeId(id("mdl", 'A')),
                1,
                true,
                facts.to_string().as_bytes(),
                'M',
            );
            chunk.error = Some(ExecutionPortError {
                code: ExecutionPortErrorCode::DeviceProviderRateLimited,
                message: "limited".to_owned(),
                retryable: true,
            });
            assert!(matches!(
                super::validate_chunk_for_retention(&chunk),
                Err(super::BridgeError::InvalidPayload)
            ));
            assert!(
                super::decode_provider_failure(
                    chunk.error.as_ref().unwrap(),
                    chunk.payload.as_ref()
                )
                .is_err()
            );
        }
        let mut chunk = test_chunk(
            &lease,
            &ModelExchangeId(id("mdl", 'A')),
            1,
            true,
            b"{\"status\":429}",
            'M',
        );
        chunk.error = Some(ExecutionPortError {
            code: ExecutionPortErrorCode::DeviceProviderRateLimited,
            message: "limited".to_owned(),
            retryable: true,
        });
        chunk.payload.as_mut().unwrap().payload_digest.0 = format!("sha256:{}", "0".repeat(64));
        assert!(
            super::decode_provider_failure(chunk.error.as_ref().unwrap(), chunk.payload.as_ref())
                .is_err()
        );
    }

    #[test]
    fn retained_authority_failures_keep_wire_codes_and_remain_terminal() {
        use winwincode_execution_port::generated::{ExecutionPortError, ExecutionPortErrorCode};

        for (code, expected) in [
            (ExecutionPortErrorCode::LeaseExpired, "LEASE_EXPIRED"),
            (ExecutionPortErrorCode::Cancelled, "CANCELLED"),
            (
                ExecutionPortErrorCode::DeviceModelPaused,
                "DEVICE_MODEL_PAUSED",
            ),
        ] {
            let failure = super::decode_provider_failure(
                &ExecutionPortError {
                    code,
                    message: "terminal".to_owned(),
                    retryable: true,
                },
                None,
            )
            .unwrap();
            assert_eq!(failure.code, expected);
            assert_eq!(failure.retryable, Some(false));
        }
    }

    #[tokio::test]
    #[allow(
        clippy::too_many_lines,
        reason = "native statistics faults must cover two calls, exact replay, restart and budget completeness"
    )]
    async fn optional_model_statistics_do_not_block_final_delivery_or_next_call() {
        let root =
            std::env::temp_dir().join(format!("wwc-optional-model-stats-{}", uuid::Uuid::now_v7()));
        let store = AdapterStore::open(&root).unwrap();
        let lease = authority(&id("cdx", 'A'));
        let source = SharedAuthoritySource::default().with_store(store.clone());
        let bridge = Arc::new(installed_loopback_bridge(
            &store,
            source,
            "optional model statistics",
            "run",
            &lease,
            "kernel",
        ));
        store
            .register_performance_run("run", ExecutionMode::React, ObserverMode::Off)
            .unwrap();
        store
            .lock()
            .unwrap()
            .execute_batch(
                "CREATE TRIGGER deny_model_statistics_insert BEFORE INSERT ON performance_operation
             BEGIN SELECT RAISE(FAIL,'optional statistics unavailable'); END;
             CREATE TRIGGER deny_model_statistics_update BEFORE UPDATE ON performance_operation
             BEGIN SELECT RAISE(FAIL,'optional statistics unavailable'); END;",
            )
            .unwrap();
        for marker in ['a', 'b'] {
            if marker == 'b' {
                store
                    .lock()
                    .unwrap()
                    .execute_batch("DROP TRIGGER deny_model_statistics_insert;")
                    .unwrap();
            }
            let request_id = format!("model-{marker}");
            let request = ModelPortRequest {
                request_id: request_id.clone(),
                payload_json: json!({
                    "requestId":request_id, "provider":"loopback", "sessionId":"kernel",
                    "threadId":lease.session_identity.codex_thread_id.0,
                    "request":{"model":"loopback", "input":[]},
                })
                .to_string(),
            };
            let mut stream = bridge.model_port().stream(request.clone()).await.unwrap();
            let open = bridge
                .take_messages()
                .unwrap()
                .into_iter()
                .find_map(|message| match message {
                    ExecutionPortMessage::ModelOpenMessage(open) => Some(open),
                    _ => None,
                })
                .unwrap();
            let chunk = final_chunk(&open, marker);
            assert!(matches!(
                bridge
                    .accept_chunk(&chunk, &lease.lease.issued_at)
                    .await
                    .unwrap(),
                ModelChunkDisposition::Delivered {
                    termination: Some(_),
                    ..
                }
            ));
            assert!(
                stream
                    .next()
                    .await
                    .unwrap()
                    .unwrap()
                    .contains("response.completed")
            );
            assert!(stream.next().await.is_none());
            assert_eq!(
                store.model_call_phase("run", &request_id).unwrap(),
                Some(ModelCallPhase::ProviderFinal)
            );
            assert!(matches!(
                bridge
                    .accept_chunk(&chunk, &lease.lease.issued_at)
                    .await
                    .unwrap(),
                ModelChunkDisposition::Duplicate { .. }
            ));
            let mut replay = bridge.model_port().stream(request).await.unwrap();
            assert!(
                replay
                    .next()
                    .await
                    .unwrap()
                    .unwrap()
                    .contains("response.completed")
            );
            assert!(replay.next().await.is_none());
            assert!(
                !bridge
                    .take_messages()
                    .unwrap()
                    .iter()
                    .any(|message| matches!(message, ExecutionPortMessage::ModelOpenMessage(_)))
            );
        }
        let reopened =
            AdapterStore::open(&root).expect("optional completion backfill cannot block restart");
        let totals = reopened.delegated_performance_totals("run").unwrap();
        assert_eq!(
            totals.primary_model_calls, 2,
            "budget counts use the durable model identities when stats are missing"
        );
        assert_eq!(totals.pending_model_calls, 0);
        assert!(!totals.usage_complete);
        assert!(!totals.cost_complete);
        assert_eq!(
            store
                .retained_outcome_usage("run", 0)
                .unwrap()
                .unwrap()
                .tokens,
            None
        );
        drop(reopened);
        drop(bridge);
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn jev_receipt_faults_keep_results_deliverable_and_accounting_unknown() {
        assert_jev_receipt_faults(&["none", "all", "partial", "source"]).await;
    }

    #[tokio::test]
    async fn pending_jev_closes_from_provider_final_when_primary_statistics_fail() {
        assert_jev_receipt_faults(&["pending"]).await;
    }

    #[allow(
        clippy::too_many_lines,
        reason = "Exercise receipt faults through the real bridge, Device store and restart"
    )]
    async fn assert_jev_receipt_faults(faults: &[&str]) {
        for &fault in faults {
            let root = std::env::temp_dir().join(format!("wwc-jev-fault-{}", uuid::Uuid::now_v7()));
            let store = AdapterStore::open(&root).unwrap();
            let lease = authority(&id("cdx", 'A'));
            let bridge = Arc::new(installed_loopback_bridge(
                &store,
                SharedAuthoritySource::default().with_store(store.clone()),
                "JEV accounting",
                "run",
                &lease,
                "kernel",
            ));
            let device_root = root.join("device");
            let device = winwincode_provider::DeviceProviderStore::open(&device_root).unwrap();
            bridge
                .attach_device_provider_directory(device_root.clone())
                .unwrap();
            let request = ModelPortRequest {
                request_id: "original-call".into(),
                payload_json: json!({
                    "requestId":"original-call", "provider":"loopback", "sessionId":"kernel",
                    "threadId":lease.session_identity.codex_thread_id.0,
                    "request":{"model":"loopback", "input":[]},
                })
                .to_string(),
            };
            let mut stream = bridge.model_port().stream(request.clone()).await.unwrap();
            let open = bridge
                .take_messages()
                .unwrap()
                .into_iter()
                .find_map(|message| match message {
                    ExecutionPortMessage::ModelOpenMessage(open) => Some(open),
                    _ => None,
                })
                .unwrap();
            bridge
                .outbox
                .retain(&ExecutionPortMessage::ModelOpenMessage(open.clone()))
                .unwrap();
            let db = rusqlite::Connection::open(device_root.join("providers.sqlite3")).unwrap();
            let saved = serde_json::to_string(&open).unwrap();
            db.execute(
                "INSERT INTO exchanges(exchange_id,digest,request_open) VALUES(?1,?2,?3)",
                rusqlite::params![
                    open.model_exchange_id.0,
                    format!("{:x}", Sha256::digest(saved.as_bytes())),
                    saved
                ],
            )
            .unwrap();
            for index in 0..2 {
                let run = if fault == "pending" {
                    None
                } else {
                    Some(json!({
                    "value":null,"observation":{"providerId":"fixture","modelId":"fixture",
                        "latency":{"secs":0,"nanos":1},"inputTokens":45,"outputTokens":5,
                        "resolvedModelId":null,"batchSize":1,"device":"remote","confidence":1.0},"failures":[],
                }).to_string())
                };
                db.execute("INSERT INTO jev_context_exchanges(operation_id,digest,result) VALUES(?1,?2,?3)",
                    rusqlite::params![format!("jev:{}:{index}",open.model_exchange_id.0),"a".repeat(64),run]).unwrap();
            }
            assert_eq!(device.model_jev_receipts(&open).unwrap().len(), 2);
            match fault {
                "all" => store.lock().unwrap().execute_batch("CREATE TRIGGER deny_jev BEFORE INSERT ON performance_jev_receipt BEGIN SELECT RAISE(FAIL,'receipt unavailable'); END;").unwrap(),
                "partial" => store.lock().unwrap().execute_batch("CREATE TRIGGER deny_jev BEFORE INSERT ON performance_jev_receipt WHEN substr(NEW.operation_id,-2)=':1' BEGIN SELECT RAISE(FAIL,'second receipt unavailable'); END;").unwrap(),
                "source" => { db.execute("UPDATE jev_context_exchanges SET result='corrupt'", []).unwrap(); },
                "pending" => store.lock().unwrap().execute_batch("CREATE TRIGGER deny_primary_completion BEFORE UPDATE ON performance_operation BEGIN SELECT RAISE(FAIL,'optional completion unavailable'); END;").unwrap(),
                "none" => {},
                _ => unreachable!(),
            }
            let mut chunk = final_chunk(&open, 'a');
            let payload = br#"{"type":"response.completed","tokenUsage":{"inputTokens":10,"outputTokens":5},"actualCostMicros":15}"#;
            chunk.payload = Some(EncodedPayload {
                content_type: "application/json".into(),
                data_base64: base64::engine::general_purpose::STANDARD.encode(payload),
                payload_digest: winwincode_domain::Sha256Digest(format!(
                    "sha256:{:x}",
                    Sha256::digest(payload)
                )),
            });
            bridge
                .accept_chunk(&chunk, &lease.lease.issued_at)
                .await
                .unwrap();
            assert!(
                stream
                    .next()
                    .await
                    .unwrap()
                    .unwrap()
                    .contains("response.completed")
            );
            assert!(stream.next().await.is_none());
            assert_eq!(
                store.model_call_phase("run", "original-call").unwrap(),
                Some(ModelCallPhase::ProviderFinal)
            );
            let imported: i64 = store
                .lock()
                .unwrap()
                .query_row("SELECT COUNT(*) FROM performance_jev_receipt", [], |row| {
                    row.get(0)
                })
                .unwrap();
            assert_eq!(
                imported,
                match fault {
                    "none" | "pending" => 2,
                    "partial" => 1,
                    _ => 0,
                }
            );
            let totals = store.delegated_performance_totals("run").unwrap();
            assert_eq!(
                totals.pending_model_calls, 0,
                "{fault}: terminal business ledger closes JEV parents"
            );
            assert_eq!(
                totals.usage_complete,
                fault == "none",
                "{fault}: missing receipts must not open token budgets"
            );
            if fault == "none" {
                assert_eq!(totals.total_tokens, 115);
            }
            assert!(
                !totals.cost_complete,
                "{fault}: missing receipts must not open cost budgets"
            );
            assert!(matches!(
                bridge
                    .accept_chunk(&chunk, &lease.lease.issued_at)
                    .await
                    .unwrap(),
                ModelChunkDisposition::Duplicate { .. }
            ));
            let mut replay = bridge.model_port().stream(request).await.unwrap();
            assert!(replay.next().await.unwrap().is_ok());
            assert!(replay.next().await.is_none());
            assert!(
                !bridge
                    .take_messages()
                    .unwrap()
                    .iter()
                    .any(|m| matches!(m, ExecutionPortMessage::ModelOpenMessage(_)))
            );
            drop(db);
            drop(device);
            drop(bridge);
            drop(store);
            let reopened = AdapterStore::open(&root).unwrap();
            let totals = reopened.delegated_performance_totals("run").unwrap();
            assert_eq!(totals.pending_model_calls, 0);
            assert_eq!(totals.usage_complete, fault == "none");
            assert!(!totals.cost_complete);
            drop(reopened);
            std::fs::remove_dir_all(root).unwrap();
        }
    }

    fn test_chunk(
        authority: &ModelLeaseAuthority,
        exchange: &ModelExchangeId,
        sequence: i64,
        is_final: bool,
        payload: &[u8],
        message_marker: char,
    ) -> ModelChunkMessage {
        ModelChunkMessage {
            error: None,
            is_final,
            kind: ModelChunkMessageKind::ModelChunk,
            lease: authority.lease.clone(),
            message_id: ExecutionMessageId(format!(
                "xmsg_0{}{message_marker}{sequence}",
                "P".repeat(20)
            )),
            model_exchange_id: exchange.clone(),
            payload: Some(EncodedPayload {
                content_type: "application/json".to_owned(),
                data_base64: base64::engine::general_purpose::STANDARD.encode(payload),
                payload_digest: winwincode_domain::Sha256Digest(format!(
                    "sha256:{:x}",
                    Sha256::digest(payload)
                )),
            }),
            schema_version: winwincode_domain::SchemaVersion::WinwincodeV1,
            sent_at: authority.lease.issued_at.clone(),
            sequence: ExecutionSequence(sequence),
            session_identity: authority.session_identity.clone(),
            worker_session_id: authority.worker_session_id.clone(),
        }
    }

    fn loopback_bridge(
        store: &AdapterStore,
        source: SharedAuthoritySource,
        outbox_label: &str,
    ) -> ExecutionPortModelBridge {
        ExecutionPortModelBridge::new(
            store.clone(),
            ExecutionOutbox::open(store.clone())
                .unwrap_or_else(|error| panic!("open {outbox_label} outbox: {error:?}")),
            ModelGatewayRoute {
                capability: "model".to_owned(),
                route: "loopback".to_owned(),
            },
            "loopback".to_owned(),
            source,
        )
    }

    fn installed_loopback_bridge(
        store: &AdapterStore,
        source: SharedAuthoritySource,
        outbox_label: &str,
        run_key: &str,
        authority: &ModelLeaseAuthority,
        kernel_session_id: &str,
    ) -> ExecutionPortModelBridge {
        let bridge = loopback_bridge(store, source, outbox_label);
        bridge
            .install_binding(ModelRunBinding {
                run_key: run_key.to_owned(),
                canonical_thread_id: authority.session_identity.codex_thread_id.clone(),
                kernel_session_id: kernel_session_id.to_owned(),
                authority: authority.clone(),
                opened_at: authority.lease.issued_at.clone(),
            })
            .unwrap_or_else(|error| panic!("install {outbox_label} binding: {error:?}"));
        bridge
    }

    #[tokio::test]
    async fn refused_model_open_closes_lifecycle_and_retains_unknown_usage_after_restart() {
        let root = std::env::temp_dir().join(format!("wwc-refused-open-{}", uuid::Uuid::now_v7()));
        let store = AdapterStore::open(&root).unwrap();
        store
            .register_performance_run("run", ExecutionMode::React, ObserverMode::Off)
            .unwrap();
        let lease = authority(&id("cdx", 'A'));
        let source = SharedAuthoritySource::default();
        let bridge = Arc::new(installed_loopback_bridge(
            &store,
            source.clone(),
            "refused open",
            "run",
            &lease,
            "kernel",
        ));
        source.update_now(&lease.lease.expires_at).unwrap();
        let request = ModelPortRequest {
            request_id: "refused-call".into(),
            payload_json: json!({
                "requestId":"refused-call", "provider":"loopback", "sessionId":"kernel",
                "threadId":lease.session_identity.codex_thread_id.0,
                "request":{"model":"loopback", "input":[]},
            })
            .to_string(),
        };
        assert!(bridge.model_port().stream(request).await.is_err());
        assert!(bridge.take_messages().unwrap().is_empty());
        assert!(bridge.exchanges.lock().unwrap().is_empty());
        assert_eq!(
            store
                .delegated_performance_totals("run")
                .unwrap()
                .pending_model_calls,
            0
        );
        let usage = store.retained_outcome_usage("run", 1).unwrap().unwrap();
        assert_eq!(usage.tokens, None);
        assert_eq!(usage.cost_microunits, None);
        drop(bridge);
        drop(store);
        let reopened = AdapterStore::open(&root).unwrap();
        assert_eq!(
            reopened.retained_outcome_usage("run", 1).unwrap(),
            Some(usage)
        );
        assert_eq!(
            reopened
                .delegated_performance_totals("run")
                .unwrap()
                .pending_model_calls,
            0
        );
        drop(reopened);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn shared_model_authority_expires_without_an_owner_clock_refresh() {
        let source = SharedAuthoritySource::default();
        let mut lease = authority(&id("cdx", 'A'));
        lease.lease.issued_at = Instant("2030-01-01T00:00:00.000Z".into());
        lease.lease.expires_at = Instant("2030-01-01T00:00:00.200Z".into());
        source.install("quiet-owner", lease.clone()).unwrap();
        source
            .validate_current(&lease, &lease.lease.issued_at)
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(250));
        assert_eq!(
            source.validate_current(&lease, &lease.lease.issued_at),
            Err(ModelAuthorityRejection::ExpiredLease)
        );
        source.update_now(&lease.lease.issued_at).unwrap();
        assert_eq!(
            source.validate_current(&lease, &lease.lease.issued_at),
            Err(ModelAuthorityRejection::ExpiredLease)
        );
        assert!(
            source
                .update_now(&Instant("2030-02-30T00:00:00.000Z".into()))
                .is_err()
        );
    }

    #[tokio::test]
    #[allow(
        clippy::too_many_lines,
        reason = "One exact expired exchange through cleanup, restart, rejection and independent job progress"
    )]
    async fn exact_expired_failure_terminal_closes_and_replays_without_blocking_other_job() {
        let root = std::env::temp_dir().join(format!("wwc-expired-final-{}", uuid::Uuid::now_v7()));
        let store = AdapterStore::open(&root).unwrap();
        let lease = authority(&id("cdx", 'A'));
        let source = SharedAuthoritySource::default();
        let bridge = Arc::new(installed_loopback_bridge(
            &store,
            source.clone(),
            "expired final",
            "run",
            &lease,
            "kernel",
        ));
        let request = ModelPortRequest {
            request_id: "expired-call".into(),
            payload_json: json!({"requestId":"expired-call", "provider":"loopback", "sessionId":"kernel", "threadId":lease.session_identity.codex_thread_id.0, "request":{"model":"loopback", "input":[]}}).to_string(),
        };
        let mut stream = bridge.model_port().stream(request).await.unwrap();
        let opens = bridge.take_messages().unwrap();
        let open = opens
            .into_iter()
            .find_map(|m| match m {
                ExecutionPortMessage::ModelOpenMessage(open) => Some(open),
                _ => None,
            })
            .unwrap();
        let mut final_frame = test_chunk(&lease, &open.model_exchange_id, 1, true, b"{}", 'A');
        final_frame.payload = None;
        final_frame.error = Some(winwincode_execution_port::generated::ExecutionPortError {
            code: winwincode_execution_port::generated::ExecutionPortErrorCode::LeaseExpired,
            message: "first start authorization expired".into(),
            retryable: false,
        });
        let now = lease.lease.expires_at.clone();
        source.update_now(&now).unwrap();
        assert!(
            bridge.accept_chunk(&final_frame, &now).await.is_err(),
            "missing durable original-request proof must reject cleanup"
        );
        bridge
            .outbox
            .retain(&ExecutionPortMessage::ModelOpenMessage(open.clone()))
            .unwrap();
        let result = bridge.accept_chunk(&final_frame, &now).await;
        assert!(
            matches!(
                result,
                Ok(ModelChunkDisposition::Delivered {
                    termination: Some(ModelTerminationReason::ProviderError),
                    ..
                })
            ),
            "{result:?}"
        );
        assert!(stream.next().await.unwrap().is_err());
        assert!(stream.next().await.is_none());
        assert!(bridge.exchanges.lock().unwrap().is_empty());
        assert!(matches!(
            bridge.accept_chunk(&final_frame, &now).await.unwrap(),
            ModelChunkDisposition::Duplicate { .. }
        ));
        let frames = store.load_model_call_frames("run", "expired-call").unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(
            store.model_call_phase("run", "expired-call").unwrap(),
            Some(ModelCallPhase::ProviderFinal)
        );
        assert!(
            !store
                .delegated_performance_totals("run")
                .unwrap()
                .usage_complete
        );
        let mut forged = final_frame.clone();
        forged.lease.fencing_token.0 = "2".into();
        assert!(bridge.accept_chunk(&forged, &now).await.is_err());
        forged = final_frame.clone();
        forged.error.as_mut().unwrap().retryable = true;
        assert!(bridge.accept_chunk(&forged, &now).await.is_err());
        forged = final_frame.clone();
        forged.error = None;
        assert!(bridge.accept_chunk(&forged, &now).await.is_err());
        forged = final_frame.clone();
        forged.worker_session_id.0 = id("wsn", 'B');
        assert!(bridge.accept_chunk(&forged, &now).await.is_err());
        forged = final_frame.clone();
        forged.session_identity.codex_thread_id.0 = id("cdx", 'B');
        assert!(bridge.accept_chunk(&forged, &now).await.is_err());
        forged = final_frame.clone();
        forged.payload =
            test_chunk(&lease, &final_frame.model_exchange_id, 1, true, b"{}", 'A').payload;
        assert!(bridge.accept_chunk(&forged, &now).await.is_err());
        forged = final_frame.clone();
        forged.sequence.0 = 2;
        assert!(bridge.accept_chunk(&forged, &now).await.is_err());
        forged = final_frame.clone();
        forged.error.as_mut().unwrap().message = "changed terminal bytes".into();
        assert!(bridge.accept_chunk(&forged, &now).await.is_err());
        assert_eq!(
            store
                .load_model_call_frames("run", "expired-call")
                .unwrap()
                .len(),
            1
        );
        let restored = loopback_bridge(
            &store,
            SharedAuthoritySource::default(),
            "restored expired final",
        );
        assert!(matches!(
            restored.accept_chunk(&final_frame, &now).await.unwrap(),
            ModelChunkDisposition::Duplicate { .. }
        ));
        assert!(restored.sink.streams.lock().unwrap().is_empty());
        drop(restored);
        let consumer_stream = frame_from_message(&ExecutionPortMessage::ModelChunkMessage(
            final_frame.clone(),
        ))
        .unwrap()
        .stream;
        let retained_source = |connection: &rusqlite::Connection| {
            let ledger: Vec<rusqlite::types::Value> = connection
                .query_row(
                    "SELECT * FROM model_call_ledger WHERE run_key=?1 AND model_call_id=?2",
                    ["run", "expired-call"],
                    |row| {
                        (0..row.as_ref().column_count())
                            .map(|column| row.get(column))
                            .collect::<rusqlite::Result<_>>()
                    },
                )
                .unwrap();
            let frames: Vec<Vec<rusqlite::types::Value>> = connection
                .prepare("SELECT * FROM model_call_frame WHERE run_key=?1 AND model_call_id=?2 ORDER BY sequence")
                .unwrap()
                .query_map(["run", "expired-call"], |row| (0..row.as_ref().column_count())
                    .map(|column| row.get(column))
                    .collect::<rusqlite::Result<_>>())
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap();
            (ledger, frames)
        };
        {
            let mut connection = store.lock().unwrap();
            let transaction = connection.transaction().unwrap();
            let source_before = retained_source(&transaction);
            assert_eq!(
                transaction
                    .execute(
                        "DELETE FROM model_cursor_frame WHERE stream_key=?1",
                        [consumer_stream.as_str()],
                    )
                    .unwrap(),
                1
            );
            assert_eq!(
                transaction
                    .execute(
                        "DELETE FROM model_cursor WHERE stream_key=?1",
                        [consumer_stream.as_str()],
                    )
                    .unwrap(),
                1
            );
            assert_eq!(retained_source(&transaction), source_before);
            transaction.commit().unwrap();
        }
        let restored = loopback_bridge(
            &store,
            SharedAuthoritySource::default(),
            "restore cursor crash window",
        );
        assert!(matches!(
            restored.accept_chunk(&final_frame, &now).await.unwrap(),
            ModelChunkDisposition::Duplicate { .. }
        ));
        assert!(matches!(
            restored.accept_chunk(&final_frame, &now).await.unwrap(),
            ModelChunkDisposition::Duplicate { .. }
        ));
        drop(restored);
        let mut next = authority(&id("cdx", 'B'));
        next.lease.job_id.0 = id("job", 'B');
        next.lease.expires_at.0 = "2030-01-01T02:00:00.000Z".into();
        bridge
            .install_binding(ModelRunBinding {
                run_key: "next".into(),
                canonical_thread_id: next.session_identity.codex_thread_id.clone(),
                kernel_session_id: "kernel-next".into(),
                authority: next.clone(),
                opened_at: now.clone(),
            })
            .unwrap();
        let mut second = bridge.model_port().stream(ModelPortRequest { request_id: "next-call".into(), payload_json: json!({"requestId":"next-call", "provider":"loopback", "sessionId":"kernel-next", "threadId":next.session_identity.codex_thread_id.0, "request":{"model":"loopback", "input":[]}}).to_string() }).await.unwrap();
        let open = bridge
            .take_messages()
            .unwrap()
            .into_iter()
            .find_map(|m| match m {
                ExecutionPortMessage::ModelOpenMessage(open) => Some(open),
                _ => None,
            })
            .unwrap();
        let success = test_chunk(
            &next,
            &open.model_exchange_id,
            1,
            true,
            br#"{"type":"response.completed","response":{}}"#,
            'B',
        );
        bridge.accept_chunk(&success, &now).await.unwrap();
        assert!(second.next().await.unwrap().is_ok());
        assert!(second.next().await.is_none());
        drop(bridge);
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn expired_failure_after_restart_rebuilds_cursor_before_first_ack() {
        let root =
            std::env::temp_dir().join(format!("wwc-detached-expiry-{}", uuid::Uuid::now_v7()));
        let store = AdapterStore::open(&root).unwrap();
        let lease = authority(&id("cdx", 'A'));
        let bridge = Arc::new(installed_loopback_bridge(
            &store,
            SharedAuthoritySource::default(),
            "before restart",
            "run",
            &lease,
            "kernel",
        ));
        let stream = bridge.model_port().stream(ModelPortRequest {
            request_id: "detached-call".into(),
            payload_json: json!({"requestId":"detached-call", "provider":"loopback", "sessionId":"kernel", "threadId":lease.session_identity.codex_thread_id.0, "request":{"model":"loopback", "input":[]}}).to_string(),
        }).await.unwrap();
        let open = bridge
            .take_messages()
            .unwrap()
            .into_iter()
            .find_map(|message| match message {
                ExecutionPortMessage::ModelOpenMessage(open) => Some(open),
                _ => None,
            })
            .unwrap();
        bridge
            .outbox
            .retain(&ExecutionPortMessage::ModelOpenMessage(open.clone()))
            .unwrap();
        let mut chunk = test_chunk(&lease, &open.model_exchange_id, 1, true, b"{}", 'A');
        chunk.payload = None;
        chunk.error = Some(winwincode_execution_port::generated::ExecutionPortError {
            code: winwincode_execution_port::generated::ExecutionPortErrorCode::LeaseExpired,
            message: "first start authority expired".into(),
            retryable: false,
        });
        drop(stream);
        drop(bridge);
        drop(store);
        let store = AdapterStore::open(&root).unwrap();
        let restored = loopback_bridge(&store, SharedAuthoritySource::default(), "after restart");
        assert!(matches!(
            restored
                .accept_chunk(&chunk, &lease.lease.expires_at)
                .await
                .unwrap(),
            ModelChunkDisposition::Delivered {
                termination: Some(ModelTerminationReason::ProviderError),
                ..
            }
        ));
        assert!(matches!(
            restored
                .accept_chunk(&chunk, &lease.lease.expires_at)
                .await
                .unwrap(),
            ModelChunkDisposition::Duplicate { .. }
        ));
        let replies = restored.take_messages().unwrap();
        assert_eq!(replies.len(), 2);
        assert!(replies.iter().all(|reply| matches!(reply, ExecutionPortMessage::ModelAckMessage(ack) if ack.error.is_none() && ack.status == winwincode_execution_port::generated::LeaseWriteStatus::Duplicate)));
        assert!(restored.sink.streams.lock().unwrap().is_empty());
        assert_eq!(
            store.model_call_phase("run", "detached-call").unwrap(),
            Some(ModelCallPhase::ProviderFinal)
        );
        assert_eq!(
            store
                .load_model_call_frames("run", "detached-call")
                .unwrap()
                .len(),
            1
        );
        drop(restored);
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }

    fn claim_test_call(
        store: &AdapterStore,
        run_key: &str,
        call_id: &str,
        exchange: &ModelExchangeId,
        request: &serde_json::Value,
    ) {
        store
            .claim_model_call(
                run_key,
                call_id,
                exchange,
                &model_call_digest(request).expect("call digest"),
            )
            .unwrap_or_else(|error| panic!("claim {call_id}: {error:?}"));
    }

    fn test_open(
        authority: &ModelLeaseAuthority,
        call_id: &str,
        exchange: &ModelExchangeId,
        request: &serde_json::Value,
    ) -> ModelOpenMessage {
        ModelOpenMessage {
            kind: winwincode_execution_port::generated::ModelOpenMessageKind::ModelOpen,
            schema_version: winwincode_domain::SchemaVersion::WinwincodeV1,
            message_id: ExecutionMessageId(id("xmsg", 'O')),
            sent_at: authority.lease.issued_at.clone(),
            request_id: winwincode_domain::RequestId(call_id.to_owned()),
            lease: authority.lease.clone(),
            worker_session_id: authority.worker_session_id.clone(),
            model_exchange_id: exchange.clone(),
            route: ModelGatewayRoute {
                capability: "model".to_owned(),
                route: "loopback".to_owned(),
            },
            request: EncodedPayload {
                content_type: "application/json".to_owned(),
                data_base64: base64::engine::general_purpose::STANDARD
                    .encode(serde_json::to_vec(request).expect("encode request")),
                payload_digest: model_call_digest(request).expect("request digest"),
            },
            session_identity: authority.session_identity.clone(),
        }
    }

    fn id(prefix: &str, value: char) -> String {
        format!("{prefix}_{}", value.to_string().repeat(26))
    }

    fn authority(thread_id: &str) -> ModelLeaseAuthority {
        let worker_session_id = WorkerSessionId(id("wsn", 'A'));
        ModelLeaseAuthority {
            lease: ExecutionLeaseStamp {
                attempt: 1,
                expires_at: Instant("2030-01-01T01:00:00.000Z".to_owned()),
                fencing_token: FencingToken("1".to_owned()),
                issued_at: Instant("2030-01-01T00:00:00.000Z".to_owned()),
                job_id: ExecutionJobId(id("job", 'A')),
                lease_id: LeaseId(id("lse", 'A')),
                worker_id: WorkerId(id("wrk", 'A')),
                worker_instance_id: WorkerInstanceId(id("wki", 'A')),
            },
            worker_session_id: worker_session_id.clone(),
            session_identity: SessionIdentity {
                codex_thread_id: CodexThreadId(thread_id.to_owned()),
                product_session_id: ProductSessionId(id("psn", 'A')),
                work_run_id: None,
                worker_session_id,
            },
        }
    }

    fn child_model_request(
        request_id: &str,
        session_id: &str,
        thread_id: &str,
        parent_thread_id: &str,
    ) -> ModelPortRequest {
        ModelPortRequest {
            request_id: request_id.to_owned(),
            payload_json: json!({
                "requestId": request_id,
                "provider": "loopback",
                "sessionId": session_id,
                "threadId": thread_id,
                "request": {
                    "model": "loopback",
                    "input": [{"role": "user", "content": request_id}],
                    "client_metadata": {
                        "x-codex-parent-thread-id": parent_thread_id,
                    },
                },
            })
            .to_string(),
        }
    }

    fn final_chunk(open: &ModelOpenMessage, message_id: char) -> ModelChunkMessage {
        let response = format!(r#"{{"type":"response.completed","call":"{message_id}"}}"#);
        let bytes = response.as_bytes();
        ModelChunkMessage {
            error: None,
            is_final: true,
            kind: ModelChunkMessageKind::ModelChunk,
            lease: open.lease.clone(),
            message_id: ExecutionMessageId(id("xmsg", message_id)),
            model_exchange_id: open.model_exchange_id.clone(),
            payload: Some(EncodedPayload {
                content_type: "application/json".to_owned(),
                data_base64: base64::engine::general_purpose::STANDARD.encode(bytes),
                payload_digest: winwincode_domain::Sha256Digest(format!(
                    "sha256:{:x}",
                    Sha256::digest(bytes)
                )),
            }),
            schema_version: winwincode_domain::SchemaVersion::WinwincodeV1,
            sent_at: open.sent_at.clone(),
            sequence: ExecutionSequence(1),
            session_identity: open.session_identity.clone(),
            worker_session_id: open.worker_session_id.clone(),
        }
    }

    #[test]
    fn provider_terminal_usage_separates_cached_from_non_cached_input() {
        let payload = json!({
            "type": "completed",
            "responseId": "response-fixture",
            "tokenUsage": {
                "input_tokens": 120,
                "cached_input_tokens": 35,
                "output_tokens": 20
            },
            "actualCostMicros": 47
        });
        let bytes = serde_json::to_vec(&payload).expect("usage payload");
        let chunk = ModelChunkMessage {
            error: None,
            is_final: true,
            kind: ModelChunkMessageKind::ModelChunk,
            lease: authority(&id("cdx", 'A')).lease,
            message_id: ExecutionMessageId(id("xmsg", 'A')),
            model_exchange_id: ModelExchangeId(id("mdl", 'A')),
            payload: Some(EncodedPayload {
                content_type: "application/json".to_owned(),
                data_base64: base64::engine::general_purpose::STANDARD.encode(&bytes),
                payload_digest: winwincode_domain::Sha256Digest(format!(
                    "sha256:{:x}",
                    Sha256::digest(&bytes)
                )),
            }),
            schema_version: winwincode_domain::SchemaVersion::WinwincodeV1,
            sent_at: Instant("2030-01-01T00:00:01.000Z".to_owned()),
            sequence: ExecutionSequence(1),
            session_identity: authority(&id("cdx", 'A')).session_identity,
            worker_session_id: WorkerSessionId(id("wsn", 'A')),
        };
        let usage = primary_model_usage(&chunk)
            .expect("decode terminal usage")
            .expect("measured usage");
        assert_eq!(usage.input, 85);
        assert_eq!(usage.cached, Some(35));
        assert_eq!(usage.output, 20);
        assert_eq!(usage.actual_cost_microunits, Some(47));

        let mut missing = chunk.clone();
        missing.payload = None;
        assert_eq!(primary_model_usage(&missing).unwrap(), None);
        let bytes = br#"{"type":"completed","endTurn":true}"#;
        missing.payload = Some(EncodedPayload {
            content_type: "application/json".into(),
            data_base64: base64::engine::general_purpose::STANDARD.encode(bytes),
            payload_digest: winwincode_domain::Sha256Digest(format!(
                "sha256:{:x}",
                Sha256::digest(bytes)
            )),
        });
        assert_eq!(primary_model_usage(&missing).unwrap(), None);

        let mut invalid = payload;
        invalid["tokenUsage"]["cached_input_tokens"] = json!(121);
        let bytes = serde_json::to_vec(&invalid).expect("invalid usage payload");
        let mut invalid_chunk = chunk;
        invalid_chunk.payload = Some(EncodedPayload {
            content_type: "application/json".to_owned(),
            data_base64: base64::engine::general_purpose::STANDARD.encode(&bytes),
            payload_digest: winwincode_domain::Sha256Digest(format!(
                "sha256:{:x}",
                Sha256::digest(&bytes)
            )),
        });
        assert_eq!(
            primary_model_usage(&invalid_chunk),
            Err(super::BridgeError::InvalidPayload)
        );
    }

    #[test]
    fn model_call_identity_ignores_transport_metadata_but_seals_model_input() {
        let original = json!({
            "model": "loopback-model",
            "input": [{"role": "user", "content": "same sealed input"}],
            "client_metadata": {
                "session_id": "original-session",
                "turn_attempt": 1
            }
        });
        let recovered = json!({
            "model": "loopback-model",
            "input": [{"role": "user", "content": "same sealed input"}],
            "client_metadata": {
                "session_id": "recovered-session",
                "turn_attempt": 2
            }
        });
        let changed_input = json!({
            "model": "loopback-model",
            "input": [{"role": "user", "content": "changed model input"}],
            "client_metadata": recovered["client_metadata"].clone()
        });

        assert_eq!(
            model_call_digest(&original).expect("original semantic model identity"),
            model_call_digest(&recovered).expect("recovered semantic model identity")
        );
        assert_ne!(
            model_call_digest(&original).expect("original semantic model identity"),
            model_call_digest(&changed_input).expect("changed semantic model identity")
        );
    }

    fn terminal_duplicate_fixture() -> (
        std::path::PathBuf,
        ExecutionPortModelBridge,
        ModelChunkMessage,
    ) {
        let root = std::env::temp_dir().join(format!(
            "winwincode-codex-terminal-duplicate-{}-{}",
            std::process::id(),
            uuid::Uuid::now_v7()
        ));
        let mut store = AdapterStore::open(&root).expect("open terminal duplicate store");
        let source = SharedAuthoritySource::default().with_store(store.clone());
        let authority = authority(&id("cdx", 'T'));
        let run_key = "terminal-run";
        let model_call_id = "terminal-call";
        let exchange = ModelExchangeId(id("mdl", 'T'));
        source
            .install(run_key, authority.clone())
            .expect("install terminal root authority");
        let bridge = ExecutionPortModelBridge::new(
            store.clone(),
            ExecutionOutbox::open(store.clone()).expect("open terminal outbox"),
            ModelGatewayRoute {
                capability: "model".to_owned(),
                route: "loopback".to_owned(),
            },
            "loopback".to_owned(),
            source,
        );
        bridge
            .install_binding(ModelRunBinding {
                run_key: run_key.to_owned(),
                canonical_thread_id: authority.session_identity.codex_thread_id.clone(),
                kernel_session_id: "kernel-terminal".to_owned(),
                authority: authority.clone(),
                opened_at: authority.lease.issued_at.clone(),
            })
            .expect("install terminal binding");
        let request =
            json!({"model": "loopback", "input": [{"role": "user", "content": "terminal"}]});
        let digest = model_call_digest(&request).expect("terminal call digest");
        store
            .claim_model_call(run_key, model_call_id, &exchange, &digest)
            .expect("claim terminal call");
        let payload_bytes = br#"{"type":"response.completed","output":[]}"#;
        let chunk = ModelChunkMessage {
            error: None,
            is_final: true,
            kind: ModelChunkMessageKind::ModelChunk,
            lease: authority.lease.clone(),
            message_id: ExecutionMessageId(id("xmsg", 'T')),
            model_exchange_id: exchange.clone(),
            payload: Some(EncodedPayload {
                content_type: "application/json".to_owned(),
                data_base64: base64::engine::general_purpose::STANDARD.encode(payload_bytes),
                payload_digest: winwincode_domain::Sha256Digest(format!(
                    "sha256:{:x}",
                    Sha256::digest(payload_bytes)
                )),
            }),
            schema_version: winwincode_domain::SchemaVersion::WinwincodeV1,
            sent_at: authority.lease.issued_at.clone(),
            sequence: ExecutionSequence(1),
            session_identity: authority.session_identity.clone(),
            worker_session_id: authority.worker_session_id.clone(),
        };
        store
            .retain_model_call_frame(run_key, model_call_id, &chunk)
            .expect("retain terminal provider frame");
        let mapped = frame_from_message(&ExecutionPortMessage::ModelChunkMessage(chunk.clone()))
            .expect("map terminal provider frame");
        store
            .record_delivery(
                &mapped.stream,
                0,
                &ModelChunkFingerprint {
                    sequence: 1,
                    message_id: chunk.message_id.clone(),
                    digest: winwincode_domain::Sha256Digest(mapped.frame.digest),
                    is_final: true,
                    has_error: false,
                },
                Some(ModelTerminationReason::Completed),
            )
            .expect("record terminal model cursor");
        store
            .mark_model_call_provider_final(run_key, model_call_id)
            .expect("mark provider terminal phase");

        (root, bridge, chunk)
    }

    #[tokio::test]
    #[allow(
        clippy::too_many_lines,
        reason = "Keep exact request, renewal and restart assertions in one end-to-end scenario"
    )]
    async fn inflight_model_response_survives_renewal_and_replays_after_restart() {
        for restart_before_response in [false, true] {
            let root =
                std::env::temp_dir().join(format!("wwc-inflight-renewal-{}", uuid::Uuid::now_v7()));
            let store = AdapterStore::open(&root).unwrap();
            let original = authority(&id("cdx", 'A'));
            let bridge = Arc::new(installed_loopback_bridge(
                &store,
                SharedAuthoritySource::default(),
                "renewal",
                "run",
                &original,
                "kernel",
            ));
            let request = ModelPortRequest {
                request_id: "inflight".into(),
                payload_json: json!({
                    "requestId":"inflight", "provider":"loopback", "sessionId":"kernel",
                    "threadId":original.session_identity.codex_thread_id.0,
                    "request":{"model":"loopback","input":[]}
                })
                .to_string(),
            };
            let mut stream = bridge.model_port().stream(request.clone()).await.unwrap();
            let open = bridge
                .take_messages()
                .unwrap()
                .into_iter()
                .find_map(|m| match m {
                    ExecutionPortMessage::ModelOpenMessage(m) => Some(m),
                    _ => None,
                })
                .unwrap();
            bridge
                .outbox
                .retain(&ExecutionPortMessage::ModelOpenMessage(open.clone()))
                .unwrap();
            let mut renewed = original.clone();
            renewed.lease.expires_at = Instant("2030-01-01T02:00:00.000Z".into());
            bridge
                .install_binding(ModelRunBinding {
                    run_key: "run".into(),
                    canonical_thread_id: renewed.session_identity.codex_thread_id.clone(),
                    kernel_session_id: "kernel".into(),
                    opened_at: renewed.lease.issued_at.clone(),
                    authority: renewed.clone(),
                })
                .unwrap();
            let now = Instant("2030-01-01T01:01:00.000Z".into());
            let bridge = if restart_before_response {
                drop(stream);
                drop(bridge);
                let restarted = Arc::new(installed_loopback_bridge(
                    &store,
                    SharedAuthoritySource::default(),
                    "inflight restart",
                    "run",
                    &renewed,
                    "kernel",
                ));
                restarted.authority.update_now(&now).unwrap();
                stream = restarted
                    .model_port()
                    .stream(request.clone())
                    .await
                    .expect("reopen original exchange under renewed lease");
                assert_eq!(
                    restarted.take_messages().unwrap(),
                    vec![ExecutionPortMessage::ModelOpenMessage(open.clone())]
                );
                restarted
            } else {
                bridge
            };
            assert!(
                bridge
                    .authority
                    .validate_exchange(&original, &ModelExchangeId(id("mdl", 'Z')), &now)
                    .is_err()
            );
            let first = test_chunk(
                &original,
                &open.model_exchange_id,
                1,
                false,
                br#"{"type":"response.created"}"#,
                'P',
            );
            let mut chunk = final_chunk(&open, 'R');
            chunk.sequence = ExecutionSequence(2);
            for change in 0..4 {
                let mut forged = chunk.clone();
                match change {
                    0 => forged.lease.expires_at = Instant("2030-01-01T01:30:00.000Z".into()),
                    1 => forged.lease.fencing_token = FencingToken("2".into()),
                    2 => forged.lease.expires_at = renewed.lease.expires_at.clone(),
                    _ => forged.session_identity.codex_thread_id = CodexThreadId(id("cdx", 'B')),
                }
                assert!(bridge.accept_chunk(&forged, &now).await.is_err());
            }
            bridge
                .accept_chunk(&first, &now)
                .await
                .expect("first old-period chunk");
            assert_eq!(
                stream.next().await,
                Some(Ok(r#"{"type":"response.created"}"#.into()))
            );
            bridge
                .outbox
                .acknowledge_response(&ExecutionPortMessage::ModelChunkMessage(first.clone()))
                .expect("compact acknowledged open from pending queue");
            assert!(bridge.outbox.pending().unwrap().is_empty());
            assert_eq!(
                bridge
                    .outbox
                    .retained_model_open(&open.model_exchange_id)
                    .unwrap(),
                Some(open.clone())
            );
            bridge
                .accept_chunk(&chunk, &now)
                .await
                .expect("original request period stays admissible under live renewal");
            assert_eq!(
                stream.next().await,
                Some(Ok(r#"{"type":"response.completed","call":"R"}"#.into()))
            );
            assert_eq!(
                store.load_model_call_frames("run", "inflight").unwrap(),
                vec![first, chunk.clone()]
            );
            assert!(bridge.take_messages().unwrap().iter().any(|m| matches!(m, ExecutionPortMessage::ModelAckMessage(ack) if ack.lease == original.lease)));
            assert!(
                matches!(
                    bridge.accept_chunk(&chunk, &now).await,
                    Ok(ModelChunkDisposition::Duplicate {
                        confirmed_sequence: 2
                    })
                ),
                "the original terminal must remain idempotent after renewal and live release"
            );
            drop(stream);
            drop(bridge);
            let restarted = Arc::new(installed_loopback_bridge(
                &store,
                SharedAuthoritySource::default(),
                "renewal restart",
                "run",
                &renewed,
                "kernel",
            ));
            restarted.authority.update_now(&now).unwrap();
            assert!(
                matches!(
                    restarted.accept_chunk(&chunk, &now).await,
                    Ok(ModelChunkDisposition::Duplicate {
                        confirmed_sequence: 2
                    })
                ),
                "restart must acknowledge the original terminal before Core reopens it"
            );
            assert!(restarted.take_messages().unwrap().iter().any(
                |message| matches!(message, ExecutionPortMessage::ModelAckMessage(ack)
                    if ack.lease == original.lease)
            ));
            let mut replay = restarted
                .model_port()
                .stream(request.clone())
                .await
                .expect("replay old exact frames with current lease");
            assert_eq!(
                replay.next().await,
                Some(Ok(r#"{"type":"response.created"}"#.into()))
            );
            assert_eq!(
                replay.next().await,
                Some(Ok(r#"{"type":"response.completed","call":"R"}"#.into()))
            );
            assert!(restarted.take_messages().unwrap().is_empty());
            assert!(matches!(
                restarted
                    .accept_chunk(&chunk, &renewed.lease.expires_at)
                    .await,
                Ok(ModelChunkDisposition::Duplicate {
                    confirmed_sequence: 2
                })
            ));
            drop(replay);
            drop(restarted);
            drop(store);
            std::fs::remove_dir_all(root).unwrap();
        }
    }

    #[tokio::test]
    async fn durable_terminal_chunk_duplicate_is_acknowledged_without_live_exchange() {
        let (root, bridge, chunk) = terminal_duplicate_fixture();
        let disposition = bridge
            .accept_chunk(&chunk, &Instant("2030-01-01T00:00:01.000Z".to_owned()))
            .await
            .expect("acknowledge terminal duplicate without process exchange");
        assert_eq!(
            disposition,
            ModelChunkDisposition::Duplicate {
                confirmed_sequence: 1,
            }
        );
        let messages = bridge
            .take_messages()
            .expect("take duplicate acknowledgement");
        assert!(messages.iter().any(|message| {
            matches!(
                message,
                ExecutionPortMessage::ModelAckMessage(ack)
                    if ack.status == winwincode_execution_port::generated::LeaseWriteStatus::Duplicate
                        && ack.ack_sequence.0 == 1
            )
        }));
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn contiguous_provider_frames_are_retained_without_a_live_process_exchange() {
        let root = std::env::temp_dir().join(format!(
            "winwincode-codex-orphan-frames-{}-{}",
            std::process::id(),
            uuid::Uuid::now_v7()
        ));
        let store = AdapterStore::open(&root).expect("open orphan frame store");
        let source = SharedAuthoritySource::default().with_store(store.clone());
        let authority = authority(&id("cdx", 'O'));
        let run_key = "orphan-run";
        let model_call_id = "orphan-call";
        let exchange = ModelExchangeId(id("mdl", 'O'));
        source
            .install(run_key, authority.clone())
            .expect("install orphan root authority");
        let bridge = loopback_bridge(&store, source, "orphan");
        bridge
            .install_binding(ModelRunBinding {
                run_key: run_key.to_owned(),
                canonical_thread_id: authority.session_identity.codex_thread_id.clone(),
                kernel_session_id: "kernel-orphan".to_owned(),
                authority: authority.clone(),
                opened_at: authority.lease.issued_at.clone(),
            })
            .expect("install orphan binding");
        let request =
            json!({"model": "loopback", "input": [{"role": "user", "content": "orphan"}]});
        let digest = model_call_digest(&request).expect("orphan call digest");
        store
            .claim_model_call(run_key, model_call_id, &exchange, &digest)
            .expect("claim orphan call");

        let chunk_one = test_chunk(
            &authority,
            &exchange,
            1,
            false,
            br#"{"type":"created"}"#,
            '1',
        );
        let chunk_two = test_chunk(
            &authority,
            &exchange,
            2,
            true,
            br#"{"type":"completed","endTurn":true}"#,
            '2',
        );

        let first = bridge
            .accept_chunk(&chunk_one, &Instant("2030-01-01T00:00:01.000Z".to_owned()))
            .await
            .expect("retain first orphan frame");
        assert_eq!(
            first,
            ModelChunkDisposition::Delivered {
                confirmed_sequence: 1,
                termination: None,
            }
        );
        let second = bridge
            .accept_chunk(&chunk_two, &Instant("2030-01-01T00:00:02.000Z".to_owned()))
            .await
            .expect("retain final orphan frame");
        assert_eq!(
            second,
            ModelChunkDisposition::Delivered {
                confirmed_sequence: 2,
                termination: Some(ModelTerminationReason::Completed),
            }
        );
        let frames = store
            .load_model_call_frames(run_key, model_call_id)
            .expect("load retained orphan frames");
        assert_eq!(frames.len(), 2);
        assert!(frames.last().expect("final orphan frame").is_final);
        assert_eq!(
            store
                .model_call_phase(run_key, model_call_id)
                .expect("read orphan call phase"),
            Some(ModelCallPhase::ProviderFinal)
        );
        let _ = std::fs::remove_dir_all(root);
    }

    fn provider_frame(
        open: &ModelOpenMessage,
        sequence: i64,
        is_final: bool,
        marker: char,
    ) -> ModelChunkMessage {
        let payload = if sequence == 1 {
            format!(r#"{{"type":"created","call":"{marker}"}}"#)
        } else if is_final {
            format!(
                r#"{{"type":"completed","endTurn":true,"call":"{marker}","protocol":"winwincode.independent-verification-result.v1"}}"#
            )
        } else {
            format!(r#"{{"type":"output_text_delta","delta":"frame-{sequence}-{marker}"}}"#)
        };
        let bytes = payload.as_bytes();
        ModelChunkMessage {
            error: None,
            is_final,
            kind: ModelChunkMessageKind::ModelChunk,
            lease: open.lease.clone(),
            message_id: ExecutionMessageId(format!("xmsg_0{}{marker}{sequence}", "P".repeat(20))),
            model_exchange_id: open.model_exchange_id.clone(),
            payload: Some(EncodedPayload {
                content_type: "application/json".to_owned(),
                data_base64: base64::engine::general_purpose::STANDARD.encode(bytes),
                payload_digest: winwincode_domain::Sha256Digest(format!(
                    "sha256:{:x}",
                    Sha256::digest(bytes)
                )),
            }),
            schema_version: winwincode_domain::SchemaVersion::WinwincodeV1,
            sent_at: open.sent_at.clone(),
            sequence: ExecutionSequence(sequence),
            session_identity: open.session_identity.clone(),
            worker_session_id: open.worker_session_id.clone(),
        }
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn post_tool_second_call_multi_frame_path_is_durably_provider_final() {
        let root = std::env::temp_dir().join(format!(
            "winwincode-codex-post-tool-second-call-{}-{}",
            std::process::id(),
            uuid::Uuid::now_v7()
        ));
        let store = AdapterStore::open(&root).expect("open post-tool store");
        let source = SharedAuthoritySource::default().with_store(store.clone());
        let authority = authority(&id("cdx", 'S'));
        let run_key = "post-tool-run";
        source
            .install(run_key, authority.clone())
            .expect("install post-tool root authority");
        let bridge = installed_loopback_bridge(
            &store,
            source,
            "post-tool",
            run_key,
            &authority,
            "kernel-post-tool",
        );

        let first_call_id = "post-tool-call-1";
        let second_call_id = "post-tool-call-2";
        let first_exchange = ModelExchangeId(id("mdl", '1'));
        let second_exchange = ModelExchangeId(id("mdl", '2'));
        let first_request =
            json!({"model": "loopback", "input": [{"role": "user", "content": "tool"}]});
        let second_request =
            json!({"model": "loopback", "input": [{"role": "user", "content": "after-tool"}]});
        claim_test_call(
            &store,
            run_key,
            first_call_id,
            &first_exchange,
            &first_request,
        );
        claim_test_call(
            &store,
            run_key,
            second_call_id,
            &second_exchange,
            &second_request,
        );

        let first_open = test_open(&authority, first_call_id, &first_exchange, &first_request);
        let second_open = ModelOpenMessage {
            request_id: winwincode_domain::RequestId(second_call_id.to_owned()),
            model_exchange_id: second_exchange.clone(),
            request: EncodedPayload {
                content_type: "application/json".to_owned(),
                data_base64: base64::engine::general_purpose::STANDARD
                    .encode(serde_json::to_vec(&second_request).unwrap()),
                payload_digest: model_call_digest(&second_request).unwrap(),
            },
            ..first_open.clone()
        };

        for sequence in 1..=3 {
            let is_final = sequence == 3;
            let disposition = bridge
                .accept_chunk(
                    &provider_frame(&first_open, sequence, is_final, 'T'),
                    &Instant(format!("2030-01-01T00:00:0{sequence}.000Z")),
                )
                .await
                .expect("retain first-call frame");
            assert!(
                matches!(disposition, ModelChunkDisposition::Delivered { .. }),
                "first call frame {sequence} must deliver"
            );
        }
        assert_eq!(
            store
                .model_call_phase(run_key, first_call_id)
                .expect("first call phase"),
            Some(ModelCallPhase::ProviderFinal)
        );

        for sequence in 1..=6 {
            let is_final = sequence == 6;
            let disposition = bridge
                .accept_chunk(
                    &provider_frame(&second_open, sequence, is_final, 'V'),
                    &Instant(format!("2030-01-01T00:01:0{sequence}.000Z")),
                )
                .await
                .unwrap_or_else(|error| {
                    panic!("second call frame {sequence} must retain: {error:?}")
                });
            assert_eq!(
                disposition,
                ModelChunkDisposition::Delivered {
                    confirmed_sequence: u64::try_from(sequence).expect("sequence"),
                    termination: is_final.then_some(ModelTerminationReason::Completed),
                },
                "second call frame {sequence}"
            );
        }

        let second_frames = store
            .load_model_call_frames(run_key, second_call_id)
            .expect("load second-call frames");
        assert_eq!(second_frames.len(), 6);
        assert_eq!(
            second_frames
                .iter()
                .map(|frame| frame.sequence.0)
                .collect::<Vec<_>>(),
            vec![1, 2, 3, 4, 5, 6]
        );
        assert!(
            second_frames
                .last()
                .expect("final second-call frame")
                .is_final
        );
        assert_eq!(
            store
                .model_call_phase(run_key, second_call_id)
                .expect("second call phase"),
            Some(ModelCallPhase::ProviderFinal)
        );
        let _ = std::fs::remove_dir_all(root);
    }

    fn sealed_profile(provider: &str, model: &str, reasoning: &str) -> serde_json::Value {
        use winwincode_execution_port::agent_config::{
            AgentProfileSettings, resolve_agent_session_config,
        };
        let capabilities = serde_json::from_value(json!({
            "capabilityDigest":format!("sha256:{}", "a".repeat(64)),
            "features":[], "maxConcurrentJobs":1, "platform":"aarch64-apple-darwin"
        }))
        .unwrap();
        let snapshot = resolve_agent_session_config(
            &WorkerId(id("wrk", 'A')),
            &capabilities,
            "executor",
            AgentProfileSettings {
                fusion: None,
                jev_judge: None,
                jev_context: None,
                provider: provider.to_owned(),
                model: model.to_owned(),
                reasoning: reasoning.to_owned(),
                tools: Vec::new(),
                sandbox: "candidate".to_owned(),
                instructions: None,
            },
        )
        .unwrap();
        serde_json::to_value(snapshot).unwrap()
    }

    #[test]
    fn core_cannot_supply_host_jev_context() {
        let (root, store, binding, route, mut request, _) = child_model_fixture();
        let bridge = ExecutionPortModelBridge::new(
            store.clone(),
            ExecutionOutbox::open(store.clone()).unwrap(),
            route,
            "loopback".to_owned(),
            SharedAuthoritySource::default(),
        );
        bridge.install_binding(binding.clone()).unwrap();
        let mut payload: serde_json::Value = serde_json::from_str(&request.payload_json).unwrap();
        for field in ["winwincodeJevContext", "winwincodeJevTask"] {
            payload[field] = json!({"provider":"injected"});
            request.payload_json = payload.to_string();
            assert!(bridge.prepare_open_stream(&request).is_err());
            assert_eq!(
                store
                    .model_call_phase(&binding.run_key, &request.request_id)
                    .unwrap(),
                None
            );
            assert!(bridge.take_messages().unwrap().is_empty());
            payload.as_object_mut().unwrap().remove(field);
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn fusion_member_routes_require_sealed_config_and_host_port_before_admission() {
        use winwincode_execution_port::agent_config::{
            AgentSessionConfigSnapshot, resolve_agent_session_config,
        };
        let (root, store, binding, route, mut request, _) = child_model_fixture();
        let base: AgentSessionConfigSnapshot =
            serde_json::from_value(sealed_profile("primary", "primary-model", "max")).unwrap();
        let mut settings = base.profile.source.settings;
        settings.fusion = Some(
            serde_json::from_value(json!({"members":[
                {"id":"glm","provider":"zhipu-glm","model":"glm-5.3-flash","reasoning":"max"},
                {"id":"mimo","provider":"xiaomi-mimo","model":"mimo-v2.6-pro","reasoning":"max"},
                {"id":"ds","provider":"deepseek","model":"deepseek-flash","reasoning":"max"},
                {"id":"qwen","provider":"opencode","model":"qwen3.8-flash","reasoning":"max"}
            ]}))
            .unwrap(),
        );
        let capabilities = serde_json::from_value(json!({"capabilityDigest":format!("sha256:{}", "a".repeat(64)),"features":[],"maxConcurrentJobs":1,"platform":"aarch64-apple-darwin"})).unwrap();
        let resolve = |settings| {
            resolve_agent_session_config(
                &WorkerId(id("wrk", 'A')),
                &capabilities,
                "executor",
                settings,
            )
        };
        let sealed = resolve(settings.clone()).unwrap();
        let mut duplicate = settings.clone();
        duplicate.fusion.as_mut().unwrap().members[1] =
            duplicate.fusion.as_ref().unwrap().members[0].clone();
        assert!(resolve(duplicate).is_err());
        let mut tampered = serde_json::to_value(&sealed).unwrap();
        tampered["profile"]["source"]["settings"]["fusion"]["members"][0]["model"] =
            json!("changed");
        store
            .save_run(&binding.run_key, &json!({"agentConfig":sealed}))
            .unwrap();
        let bridge = ExecutionPortModelBridge::new(
            store.clone(),
            ExecutionOutbox::open(store.clone()).unwrap(),
            route,
            "primary".into(),
            SharedAuthoritySource::default(),
        );
        bridge.install_binding(binding.clone()).unwrap();
        let mut payload: serde_json::Value = serde_json::from_str(&request.payload_json).unwrap();
        payload["provider"] = json!("zhipu-glm");
        payload["request"]["model"] = json!("glm-5.3-flash");
        payload["request"]["reasoning"] = json!({"effort":"max"});
        request.payload_json = payload.to_string();
        assert!(bridge.prepare_open_stream(&request).is_err());
        assert!(
            bridge
                .prepare_member_stream(&request, Some("unknown"))
                .is_err()
        );
        assert!(
            bridge
                .prepare_member_stream(&request, Some("mimo"))
                .is_err()
        );
        let mut wrong_effort = request.clone();
        payload["request"]["reasoning"]["effort"] = json!("high");
        wrong_effort.payload_json = payload.to_string();
        assert!(
            bridge
                .prepare_member_stream(&wrong_effort, Some("glm"))
                .is_err()
        );
        assert!(bridge.take_messages().unwrap().is_empty());
        assert_eq!(
            store
                .model_call_phase(&binding.run_key, &request.request_id)
                .unwrap(),
            None
        );
        store
            .save_run(&binding.run_key, &json!({"agentConfig":tampered}))
            .unwrap();
        assert!(bridge.prepare_member_stream(&request, Some("glm")).is_err());
        store
            .save_run(
                &binding.run_key,
                &json!({"agentConfig":resolve(settings).unwrap()}),
            )
            .unwrap();
        let super::OpenStreamPreparation::Open(exchange) =
            bridge.prepare_member_stream(&request, Some("glm")).unwrap()
        else {
            panic!("first member request must open");
        };
        let command = bridge.open_command(&exchange).unwrap();
        futures::executor::block_on(async {
            bridge.client.lock().await.open(command).await.unwrap();
        });
        for message in bridge.take_messages().unwrap() {
            bridge.outbox.retain(&message).unwrap();
        }
        assert!(bridge.open_command(&exchange).is_ok());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn model_provider_is_bound_to_the_selected_run_instead_of_the_worker_default() {
        let (root, store, binding, route, mut request, _) = child_model_fixture();
        store
            .save_run(
                &binding.run_key,
                &json!({
                    "agentConfig": sealed_profile("selected", "loopback", "provider_default")
                }),
            )
            .expect("persist selected provider");
        let bridge = ExecutionPortModelBridge::new(
            store.clone(),
            ExecutionOutbox::open(store).unwrap(),
            route,
            "loopback".to_owned(),
            SharedAuthoritySource::default(),
        );
        bridge.install_binding(binding).unwrap();
        let mut payload: serde_json::Value = serde_json::from_str(&request.payload_json).unwrap();
        payload["provider"] = json!("selected");
        request.payload_json = payload.to_string();
        assert!(bridge.prepare_open_stream(&request).is_ok());
        payload["provider"] = json!("loopback");
        request.payload_json = payload.to_string();
        assert!(bridge.prepare_open_stream(&request).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn frozen_model_and_reasoning_are_checked_before_model_call_admission() {
        let (root, store, binding, route, request, _) = child_model_fixture();
        store
            .save_run(
                &binding.run_key,
                &json!({
                    "agentConfig": sealed_profile("loopback", "selected-model", "max")
                }),
            )
            .unwrap();
        let bridge = ExecutionPortModelBridge::new(
            store.clone(),
            ExecutionOutbox::open(store.clone()).unwrap(),
            route,
            "loopback".to_owned(),
            SharedAuthoritySource::default(),
        );
        bridge.install_binding(binding.clone()).unwrap();
        let original: serde_json::Value = serde_json::from_str(&request.payload_json).unwrap();
        for (model, effort) in [
            (json!("different-model"), json!("max")),
            (json!("selected-model"), json!("high")),
            (json!("selected-model"), serde_json::Value::Null),
            (serde_json::Value::Null, json!("max")),
        ] {
            let mut payload = original.clone();
            payload["request"]["model"] = model;
            payload["request"]["reasoning"] = json!({"effort": effort});
            let mut changed = request.clone();
            changed.payload_json = payload.to_string();
            let failure = bridge
                .prepare_open_stream(&changed)
                .err()
                .expect("unsealed settings rejected");
            assert_eq!(failure.code, "INVALID_REQUEST");
            assert_eq!(
                store
                    .model_call_phase(&binding.run_key, &request.request_id)
                    .unwrap(),
                None
            );
            assert!(bridge.take_messages().unwrap().is_empty());
        }
        let mut payload = original;
        payload["request"]["model"] = json!("selected-model");
        payload["request"]["reasoning"] = json!({"effort": "max"});
        let mut accepted = request;
        accepted.payload_json = payload.to_string();
        assert!(bridge.prepare_open_stream(&accepted).is_ok());
        std::fs::remove_dir_all(root).unwrap();
    }

    fn child_model_fixture() -> (
        std::path::PathBuf,
        AdapterStore,
        ModelRunBinding,
        ModelGatewayRoute,
        ModelPortRequest,
        ModelPortRequest,
    ) {
        let root = std::env::temp_dir().join(format!(
            "winwincode-codex-child-model-calls-{}-{}",
            std::process::id(),
            uuid::Uuid::now_v7()
        ));
        let store = AdapterStore::open(&root).expect("open child model-call store");
        store
            .register_performance_run("child-run", ExecutionMode::React, ObserverMode::Off)
            .expect("register child performance run");
        let root_authority = authority(&id("cdx", 'R'));
        let root_binding = ModelRunBinding {
            run_key: "child-run".to_owned(),
            canonical_thread_id: root_authority.session_identity.codex_thread_id.clone(),
            kernel_session_id: "kernel-root".to_owned(),
            authority: root_authority.clone(),
            opened_at: root_authority.lease.issued_at.clone(),
        };
        let route = ModelGatewayRoute {
            capability: "model".to_owned(),
            route: "loopback".to_owned(),
        };
        let request_a = child_model_request(
            "child-call-a",
            "kernel-child-a",
            &id("cdx", 'A'),
            &id("cdx", 'R'),
        );
        let request_b = child_model_request(
            "child-call-b",
            "kernel-child-b",
            &id("cdx", 'B'),
            &id("cdx", 'R'),
        );
        (root, store, root_binding, route, request_a, request_b)
    }

    async fn open_parallel_child_calls(
        store: &AdapterStore,
        root_binding: &ModelRunBinding,
        route: &ModelGatewayRoute,
        request_a: &ModelPortRequest,
        request_b: &ModelPortRequest,
    ) -> (ModelOpenMessage, ModelOpenMessage, (Instant, Instant)) {
        let source = SharedAuthoritySource::default().with_store(store.clone());
        let bridge = Arc::new(ExecutionPortModelBridge::new(
            store.clone(),
            ExecutionOutbox::open(store.clone()).expect("open child model-call outbox"),
            route.clone(),
            "loopback".to_owned(),
            source.clone(),
        ));
        bridge
            .install_binding(root_binding.clone())
            .expect("install child model-call bridge root");
        let before_open = source
            .observed_now()
            .expect("observe clock before child calls")
            .expect("installed child authority clock");
        let model_port = bridge.model_port();
        let (stream_a, stream_b) = tokio::join!(
            model_port.stream(request_a.clone()),
            model_port.stream(request_b.clone()),
        );
        let after_open = source
            .observed_now()
            .expect("observe clock after child calls")
            .expect("installed child authority clock");
        drop(stream_a.expect("open first child model stream"));
        drop(stream_b.expect("open second child model stream"));
        let messages = bridge
            .take_messages()
            .expect("take parallel child model opens");
        let primary = messages.iter().find_map(|message| match message {
            ExecutionPortMessage::ModelOpenMessage(open)
                if open.session_identity.codex_thread_id == CodexThreadId(id("cdx", 'A')) =>
            {
                Some(open.clone())
            }
            _ => None,
        });
        let secondary = messages.iter().find_map(|message| match message {
            ExecutionPortMessage::ModelOpenMessage(open)
                if open.session_identity.codex_thread_id == CodexThreadId(id("cdx", 'B')) =>
            {
                Some(open.clone())
            }
            _ => None,
        });
        (
            primary.expect("first child ModelOpen"),
            secondary.expect("second child ModelOpen"),
            (before_open, after_open),
        )
    }

    async fn complete_restarted_child_calls(
        store: &AdapterStore,
        root_binding: &ModelRunBinding,
        route: &ModelGatewayRoute,
        request_a: &ModelPortRequest,
        request_b: &ModelPortRequest,
        primary_open: &ModelOpenMessage,
        secondary_open: &ModelOpenMessage,
    ) {
        let restarted_source = SharedAuthoritySource::default().with_store(store.clone());
        let bridge = Arc::new(ExecutionPortModelBridge::new(
            store.clone(),
            ExecutionOutbox::open(store.clone()).expect("open child replay outbox"),
            route.clone(),
            "loopback".to_owned(),
            restarted_source,
        ));
        bridge
            .install_binding(root_binding.clone())
            .expect("reinstall child model-call bridge root");
        let model_port = bridge.model_port();
        let (primary_result, secondary_result) = tokio::join!(
            model_port.stream(request_a.clone()),
            model_port.stream(request_b.clone()),
        );
        let mut primary_stream = primary_result.expect("reopen first child model stream");
        let mut secondary_stream = secondary_result.expect("reopen second child model stream");
        let messages = bridge
            .take_messages()
            .expect("take restarted child model opens");
        let reopened_primary = messages.iter().find_map(|message| match message {
            ExecutionPortMessage::ModelOpenMessage(open)
                if open.session_identity.codex_thread_id == CodexThreadId(id("cdx", 'A')) =>
            {
                Some(open.clone())
            }
            _ => None,
        });
        let reopened_secondary = messages.iter().find_map(|message| match message {
            ExecutionPortMessage::ModelOpenMessage(open)
                if open.session_identity.codex_thread_id == CodexThreadId(id("cdx", 'B')) =>
            {
                Some(open.clone())
            }
            _ => None,
        });
        let primary = reopened_primary.expect("restarted first child ModelOpen");
        let secondary = reopened_secondary.expect("restarted second child ModelOpen");
        assert_eq!(&primary, primary_open);
        assert_eq!(&secondary, secondary_open);
        bridge
            .accept_chunk(
                &final_chunk(&primary, 'a'),
                &Instant("2030-01-01T00:01:00.000Z".to_owned()),
            )
            .await
            .expect("accept first child ProviderFinal chunk");
        bridge
            .accept_chunk(
                &final_chunk(&secondary, 'b'),
                &Instant("2030-01-01T00:01:00.000Z".to_owned()),
            )
            .await
            .expect("accept second child ProviderFinal chunk");
        assert_eq!(
            primary_stream.next().await,
            Some(Ok(r#"{"type":"response.completed","call":"a"}"#.to_owned()))
        );
        assert_eq!(
            secondary_stream.next().await,
            Some(Ok(r#"{"type":"response.completed","call":"b"}"#.to_owned()))
        );
        assert!(primary_stream.next().await.is_none());
        assert!(secondary_stream.next().await.is_none());
    }

    async fn replay_final_child_calls(
        store: &AdapterStore,
        root_binding: &ModelRunBinding,
        route: &ModelGatewayRoute,
    ) {
        let restarted_source = SharedAuthoritySource::default().with_store(store.clone());
        let bridge = Arc::new(ExecutionPortModelBridge::new(
            store.clone(),
            ExecutionOutbox::open(store.clone()).expect("open child replay-after-final outbox"),
            route.clone(),
            "loopback".to_owned(),
            restarted_source,
        ));
        bridge
            .install_binding(root_binding.clone())
            .expect("reinstall child replay-after-final bridge root");
        let model_port = bridge.model_port();
        let mut replay_primary = model_port
            .stream(child_model_request(
                "child-call-a",
                "kernel-child-a",
                &id("cdx", 'A'),
                &id("cdx", 'R'),
            ))
            .await
            .expect("reopen first child ProviderFinal stream");
        let mut replay_secondary = model_port
            .stream(child_model_request(
                "child-call-b",
                "kernel-child-b",
                &id("cdx", 'B'),
                &id("cdx", 'R'),
            ))
            .await
            .expect("reopen second child ProviderFinal stream");
        assert!(
            bridge
                .take_messages()
                .expect("take child replay-after-final messages")
                .iter()
                .all(|message| !matches!(message, ExecutionPortMessage::ModelOpenMessage(_))),
            "ProviderFinal calls must replay without a new ModelOpen"
        );
        assert_eq!(
            replay_primary.next().await,
            Some(Ok(r#"{"type":"response.completed","call":"a"}"#.to_owned()))
        );
        assert_eq!(
            replay_secondary.next().await,
            Some(Ok(r#"{"type":"response.completed","call":"b"}"#.to_owned()))
        );
    }

    fn observed_child_model_wait(
        store: &AdapterStore,
        before_open: &Instant,
        after_open: &Instant,
    ) -> (Vec<(String, String)>, i64) {
        use time::{OffsetDateTime, format_description::well_known::Rfc3339};

        let started: Vec<(String, String)> = store
            .lock()
            .expect("read child model start observations")
            .prepare("SELECT operation_id,started_at FROM performance_operation WHERE run_key='child-run' AND operation_kind='primary_model' ORDER BY operation_id")
            .expect("prepare child model start observations")
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .expect("query child model start observations")
            .collect::<Result<_, _>>()
            .expect("retain child model start observations");
        assert_eq!(
            started
                .iter()
                .map(|(call, _)| call.as_str())
                .collect::<Vec<_>>(),
            vec!["child-call-a", "child-call-b"]
        );
        let completed_at = OffsetDateTime::parse("2030-01-01T00:01:00.000Z", &Rfc3339)
            .expect("parse fixture terminal observation");
        let expected_wait_ms: i64 = started
            .iter()
            .map(|(_, at)| {
                assert!(
                    before_open.0.as_str() <= at.as_str() && at.as_str() <= after_open.0.as_str()
                );
                let began = OffsetDateTime::parse(at, &Rfc3339)
                    .expect("parse actual child model start observation");
                assert!(began < completed_at);
                i64::try_from((completed_at - began).whole_milliseconds())
                    .expect("bounded child model duration")
            })
            .sum();
        (started, expected_wait_ms)
    }

    #[tokio::test]
    async fn child_model_calls_bind_independent_exchanges_and_replay_exactly_after_restart() {
        let (root, store, root_binding, route, request_a, request_b) = child_model_fixture();
        let (primary_open, secondary_open, (before_open, after_open)) =
            open_parallel_child_calls(&store, &root_binding, &route, &request_a, &request_b).await;
        let (started, expected_wait_ms) =
            observed_child_model_wait(&store, &before_open, &after_open);
        assert_ne!(
            primary_open.model_exchange_id,
            secondary_open.model_exchange_id
        );
        assert_eq!(
            store
                .model_call_phase("child-run", "child-call-a")
                .expect("first child phase"),
            Some(ModelCallPhase::InFlight)
        );
        assert_eq!(
            store
                .model_call_phase("child-run", "child-call-b")
                .expect("second child phase"),
            Some(ModelCallPhase::InFlight)
        );
        complete_restarted_child_calls(
            &store,
            &root_binding,
            &route,
            &request_a,
            &request_b,
            &primary_open,
            &secondary_open,
        )
        .await;
        assert_eq!(
            store
                .model_call_phase("child-run", "child-call-a")
                .expect("first child terminal phase"),
            Some(ModelCallPhase::ProviderFinal)
        );
        assert_eq!(
            store
                .model_call_phase("child-run", "child-call-b")
                .expect("second child terminal phase"),
            Some(ModelCallPhase::ProviderFinal)
        );
        let before_replay = store
            .performance_report("child-run", 60_000)
            .expect("child performance report before final replay");
        assert_eq!(before_replay.primary_model_call_count, 2);
        assert_eq!(before_replay.primary_model_wait_ms, expected_wait_ms);
        replay_final_child_calls(&store, &root_binding, &route).await;
        let report = store
            .performance_report("child-run", 60_000)
            .expect("child performance report after restart replay");
        assert_eq!(report.primary_model_call_count, 2);
        assert_eq!(report.primary_model_wait_ms, expected_wait_ms);
        assert_eq!(
            report.primary_model_wait_ms,
            before_replay.primary_model_wait_ms
        );
        eprintln!(
            "MODEL_MECHANISM_AUDIT {}",
            json!({
                "actual_path":"advancing AuthorityClock -> child ModelOpen -> retained performance start/completion -> final replay",
                "before_open":before_open.0,
                "after_open":after_open.0,
                "actual_call_start_observations":started,
                "terminal_observation":"2030-01-01T00:01:00.000Z",
                "expected_wait_ms":expected_wait_ms,
                "actual_wait_ms":report.primary_model_wait_ms,
                "replay_preserved_wait_ms":before_replay.primary_model_wait_ms,
            })
        );
        assert_eq!(
            store
                .delegated_performance_totals("child-run")
                .unwrap()
                .pending_model_calls,
            0
        );
        assert_eq!(
            store.retained_outcome_usage("child-run", 60_000).unwrap(),
            Some(winwincode_execution_port::generated::ExecutionOutcomeUsage::unknown(60_000, 0))
        );
        assert_eq!(report.primary_model_input_tokens, None);
        assert_eq!(report.primary_model_cached_tokens, None);
        assert_eq!(report.primary_model_output_tokens, 0);
        std::fs::remove_dir_all(root).expect("remove child model-call store");
    }

    #[test]
    fn root_extension_preserves_model_child_aliases() {
        let (root, store, original, _, _, _) = child_model_fixture();
        let bridge = installed_loopback_bridge(
            &store,
            SharedAuthoritySource::default(),
            "renew model aliases",
            &original.run_key,
            &original.authority,
            &original.kernel_session_id,
        );
        let mut child = original.clone();
        child.canonical_thread_id = CodexThreadId(id("cdx", 'C'));
        child.kernel_session_id = "kernel-child".to_owned();
        child.authority.session_identity.codex_thread_id = child.canonical_thread_id.clone();
        bridge
            .install_child_binding(child.clone())
            .expect("install child");
        let mut renewed = original;
        renewed.authority.lease.expires_at = Instant("2030-01-01T02:00:00.000Z".to_owned());
        bridge
            .install_binding(renewed.clone())
            .expect("extend root");
        bridge
            .install_binding(renewed.clone())
            .expect("replay extension");
        {
            let bindings = bridge.bindings.read().expect("bindings");
            for alias in [&child.kernel_session_id, &child.canonical_thread_id.0] {
                let current = bindings.get(alias).expect("child alias retained");
                assert_eq!(
                    current.authority.lease.expires_at,
                    renewed.authority.lease.expires_at
                );
                assert_eq!(current.canonical_thread_id, child.canonical_thread_id);
            }
        }
        renewed.authority.lease.attempt += 1;
        renewed.authority.lease.fencing_token = FencingToken("2".to_owned());
        bridge.install_binding(renewed).expect("replace attempt");
        assert!(
            !bridge
                .bindings
                .read()
                .expect("bindings")
                .contains_key(&child.kernel_session_id)
        );
        std::fs::remove_dir_all(root).expect("remove model alias fixture");
    }

    #[test]
    fn child_authority_lineage_survives_source_restart() {
        let root = std::env::temp_dir().join(format!(
            "winwincode-codex-lineage-{}-{}",
            std::process::id(),
            uuid::Uuid::now_v7()
        ));
        let store = AdapterStore::open(&root).expect("open lineage store");
        let source = SharedAuthoritySource::default().with_store(store.clone());
        let root_authority = authority(&id("cdx", 'R'));
        source
            .install("run", root_authority.clone())
            .expect("install root authority");
        let mut child_authority = root_authority.clone();
        child_authority.session_identity.codex_thread_id = CodexThreadId(id("cdx", 'C'));
        source
            .install_child("run", child_authority.clone())
            .expect("install child authority");
        source
            .install("run", root_authority.clone())
            .expect("idempotent root install preserves child authority");
        assert!(
            source
                .validate_current(
                    &child_authority,
                    &Instant("2030-01-01T00:01:00.000Z".to_owned())
                )
                .is_ok()
        );

        let restarted = SharedAuthoritySource::default().with_store(store);
        restarted
            .install("run", root_authority)
            .expect("reinstall root without dropping child lineage");
        assert!(
            restarted
                .validate_current(
                    &child_authority,
                    &Instant("2030-01-01T00:01:00.000Z".to_owned())
                )
                .is_ok(),
            "reinstalling the exact root must retain durable child lineage"
        );
        restarted
            .install_child("run", child_authority.clone())
            .expect("reinstall child from durable root lineage");
        assert!(
            restarted
                .validate_current(
                    &child_authority,
                    &Instant("2030-01-01T00:01:00.000Z".to_owned())
                )
                .is_ok()
        );
        restarted
            .remove(&id("job", 'A'), "run")
            .expect("remove run lineage");
        assert!(
            restarted
                .validate_current(
                    &child_authority,
                    &Instant("2030-01-01T00:01:00.000Z".to_owned())
                )
                .is_err()
        );
        std::fs::remove_dir_all(root).expect("remove lineage store");
    }

    #[test]
    fn lease_extension_preserves_child_authority_across_restart() {
        let root = std::env::temp_dir().join(format!("codex-renew-{}", uuid::Uuid::now_v7()));
        let store = AdapterStore::open(&root).unwrap();
        let source = SharedAuthoritySource::default().with_store(store.clone());
        let original = authority(&id("cdx", 'R'));
        source.install("run", original.clone()).unwrap();
        let mut child = original.clone();
        child.session_identity.codex_thread_id = CodexThreadId(id("cdx", 'C'));
        source.install_child("run", child.clone()).unwrap();
        let mut renewed = original.clone();
        renewed.lease.expires_at = Instant("2030-01-01T02:00:00.000Z".into());
        source.install("run", renewed.clone()).unwrap();
        let after_prior_expiry = Instant("2030-01-01T01:30:00.000Z".into());
        source.update_now(&after_prior_expiry).unwrap();
        assert!(
            source
                .validate_current(&child, &after_prior_expiry)
                .is_err()
        );
        child.lease = renewed.lease.clone();
        assert!(source.validate_current(&child, &after_prior_expiry).is_ok());
        let restarted = SharedAuthoritySource::default().with_store(store);
        restarted.install("run", renewed.clone()).unwrap();
        restarted.update_now(&after_prior_expiry).unwrap();
        assert!(
            restarted
                .validate_current(&child, &after_prior_expiry)
                .is_ok()
        );
        renewed.lease.attempt += 1;
        renewed.lease.fencing_token = FencingToken("2".into());
        restarted.install("next-run", renewed).unwrap();
        assert!(
            restarted
                .validate_current(&child, &after_prior_expiry)
                .is_err()
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn replacing_root_fences_old_durable_child_lineage() {
        let root = std::env::temp_dir().join(format!(
            "winwincode-codex-lineage-replace-{}-{}",
            std::process::id(),
            uuid::Uuid::now_v7()
        ));
        let store = AdapterStore::open(&root).expect("open lineage store");
        let source = SharedAuthoritySource::default().with_store(store);
        let root_authority = authority(&id("cdx", 'R'));
        source
            .install("old-run", root_authority.clone())
            .expect("install old root authority");
        let mut old_child = root_authority.clone();
        old_child.session_identity.codex_thread_id = CodexThreadId(id("cdx", 'C'));
        source
            .install_child("old-run", old_child.clone())
            .expect("install old child authority");

        let mut replacement = authority(&id("cdx", 'N'));
        replacement.lease.attempt = 2;
        replacement.lease.fencing_token = FencingToken("2".to_owned());
        replacement.lease.lease_id = LeaseId(id("lse", 'B'));
        replacement.lease.issued_at = Instant("2030-01-01T02:00:00.000Z".to_owned());
        replacement.lease.expires_at = Instant("2030-01-01T03:00:00.000Z".to_owned());
        source
            .install("new-run", replacement.clone())
            .expect("install replacement root authority");
        assert_eq!(
            source.install_child("old-run", old_child),
            Err(super::BridgeError::StaleAuthority)
        );

        let mut new_child = replacement.clone();
        new_child.session_identity.codex_thread_id = CodexThreadId(id("cdx", 'D'));
        source
            .install_child("new-run", new_child.clone())
            .expect("install replacement child authority");
        assert!(
            source
                .validate_current(&new_child, &Instant("2030-01-01T02:01:00.000Z".to_owned()))
                .is_ok()
        );
        std::fs::remove_dir_all(root).expect("remove lineage store");
    }
}
