// SPDX-License-Identifier: Apache-2.0

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::future::Future;
use std::sync::{Arc, Mutex, MutexGuard, RwLock};

use crate::outbox::ExecutionOutbox;
use crate::{
    WorkerExecutionPort,
    action_bridge::ExecutionPortActionGate,
    model_port_client::{
        ModelAuthorityRejection, ModelChunkDelivery, ModelChunkDisposition, ModelChunkSink,
        ModelLeaseAuthority, ModelLeaseAuthoritySource, ModelMessageMetadata,
        ModelSinkDeliveryStatus, ModelTerminationReason, OpenModelExchangeCommand,
        WorkerModelPortClient,
    },
};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use futures::{future::BoxFuture, stream};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::sync::{Mutex as AsyncMutex, mpsc};
use winwincode_domain::{CodexThreadId, ExecutionMessageId, Instant, ModelExchangeId, RequestId};
use winwincode_execution_port::{
    generated::{EncodedPayload, ExecutionPortMessage, ModelChunkMessage, ModelGatewayRoute},
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
    /// Exact authority for every Core thread in a run, including spawned
    /// descendants.  The job-key map above remains the trusted root lease;
    /// this map prevents a child from inheriting authority merely because it
    /// carries the same job id.
    lineage: Arc<RwLock<HashMap<String, ModelLeaseAuthority>>>,
    /// Worker-owned durable backing for exact root/child lineage.  Tests that
    /// exercise only the protocol source can use the default in-memory mode.
    store: Option<AdapterStore>,
    /// Latest Worker-observed trusted clock.  The open frame keeps its
    /// original transport timestamp for replay, while this value prevents a
    /// stale timestamp from being used to authorize a later Provider call.
    trusted_now: Arc<RwLock<Option<Instant>>>,
}

impl SharedAuthoritySource {
    pub(crate) fn with_store(mut self, store: AdapterStore) -> Self {
        self.store = Some(store);
        self
    }

    pub(crate) fn update_now(&self, now: &Instant) -> Result<(), BridgeError> {
        let mut trusted_now = self
            .trusted_now
            .write()
            .map_err(|_| BridgeError::Unavailable)?;
        if trusted_now
            .as_ref()
            .is_none_or(|current| current.0.as_str() < now.0.as_str())
        {
            *trusted_now = Some(now.clone());
        }
        Ok(())
    }

    pub(crate) fn observed_now(&self) -> Result<Option<Instant>, BridgeError> {
        self.trusted_now
            .read()
            .map(|now| now.clone())
            .map_err(|_| BridgeError::Unavailable)
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
            .is_some_and(|existing| existing == &authority);
        let persisted_root_same =
            self.load_lineage_binding(&thread_id)?
                .is_some_and(|(existing_run_key, existing)| {
                    existing_run_key == run_key && existing == authority
                });
        // A durable source is authoritative for run identity.  The
        // process-local job map stores only the lease authority, so it cannot
        // make a root with a different run key look idempotent.
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
        // A replacement lease invalidates all old descendants before a new
        // root is made available.  Never leave an old child bound to a new
        // attempt/fencing token.
        if !same_root {
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
}

impl ModelLeaseAuthoritySource for SharedAuthoritySource {
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
        let trusted_now = self
            .trusted_now
            .read()
            .map_err(|_| ModelAuthorityRejection::Unavailable)?
            .clone()
            .unwrap_or_else(|| now.clone());
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
        if expected != presented
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
        if streams.insert(exchange.0.clone(), sender).is_some() {
            return Err(BridgeError::Conflict);
        }
        Ok(receiver)
    }

    fn remove(&self, exchange: &ModelExchangeId) -> Result<(), BridgeError> {
        self.lock()?.remove(&exchange.0);
        Ok(())
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
            Err(ModelPortFailure::new(
                format!("{:?}", error.code),
                "Provider Gateway returned a terminal model error",
            ))
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
    queue: QueuedModelPort,
    sink: KernelStreamSink,
    authority: SharedAuthoritySource,
    client: AsyncMutex<ModelClient>,
    ordinal_store: AdapterStore,
    outbox: ExecutionOutbox,
    bindings: RwLock<HashMap<String, ModelRunBinding>>,
    exchanges: Mutex<HashMap<String, ModelExchangeOwner>>,
    /// The action gate is attached after both sides are constructed.  A
    /// model stream can discover a durable child lineage and immediately
    /// reach a Core tool handler in the same `next_event` call; installing
    /// the binding only after that call returns leaves a TOCTOU window where
    /// the child is known to the model bridge but not to the action gate.
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
        })
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
        // An idempotent root rebind after restart must not fence child
        // aliases which were already restored for the same immutable
        // authority.  A changed root remains the only operation that fences
        // the full run lineage.
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
            // A replacement lease fences every descendant of the preceding
            // attempt. Drop its process-local aliases before inserting the
            // new root so stale child requests cannot resolve to an old
            // authority during the replacement window.
            let job_id = binding.authority.lease.job_id.clone();
            bindings.retain(|_, existing| {
                existing.authority.lease.job_id != job_id || same_binding(existing, &binding)
            });
        }
        bindings.insert(binding.kernel_session_id.clone(), binding.clone());
        bindings.insert(binding.canonical_thread_id.0.clone(), binding);
        Ok(())
    }

    /// Detaches the live model binding while retaining the exact replay
    /// authority for durable terminal deliveries. A Worker may close the
    /// embedded Core session before the Control Plane acknowledges the final
    /// `RuntimeEvent` or `JobOutcome`; those acknowledgements still need the
    /// original lease/session lineage for validation. A later attempt replaces
    /// this authority through `install_binding`, invalidating the predecessor.
    pub(crate) fn detach_binding(&self, binding: &ModelRunBinding) -> Result<(), BridgeError> {
        self.bindings
            .write()
            .map_err(|_| BridgeError::Unavailable)?
            .retain(|_, candidate| candidate.run_key != binding.run_key);
        Ok(())
    }

    pub(crate) async fn accept_chunk(
        &self,
        chunk: &ModelChunkMessage,
        received_at: &Instant,
    ) -> Result<ModelChunkDisposition, BridgeError> {
        let (owner, binding, process_exchange) = self.resolve_chunk_owner(chunk)?;
        let metadata = ModelMessageMetadata {
            message_id: execution_message_id(b"model-ack", &chunk.message_id.0),
            sent_at: received_at.clone(),
        };
        self.validate_chunk_authority(chunk, &binding, received_at)?;
        // The sink can wake Core synchronously while `accept_chunk` is
        // awaiting. Advance the action gate only after the complete chunk
        // authority check succeeds, but before the frame is delivered to
        // Core, so a tool request cannot race with the trusted-clock update.
        self.sync_action_clock(received_at)?;
        // Keep the exact response frame before handing it to Core.  This
        // closes the crash window where Core has consumed a final chunk but
        // the private replay ledger has not yet been written.  A gap or a
        // changed duplicate is left to WorkerModelPortClient so it can emit
        // the canonical Gap/Conflict acknowledgement instead of poisoning
        // the durable response sequence.
        let mut client = self.client.lock().await;
        let persisted_frames = self
            .ordinal_store
            .load_model_call_frames(&owner.run_key, &owner.model_call_id)
            .map_err(model_frame_store_error)?;
        let sequence = usize::try_from(chunk.sequence.0).ok();
        let expected = persisted_frames.len().saturating_add(1);
        let exact_duplicate = sequence
            .and_then(|sequence| sequence.checked_sub(1))
            .and_then(|index| persisted_frames.get(index))
            .is_some_and(|existing| existing == chunk);
        if !process_exchange {
            // A Provider may retry a terminal frame after the Worker process
            // has released its live Core stream.  The durable frame ledger is
            // authoritative in that interval: acknowledge only an exact
            // retained frame and never create a new Provider/Core exchange.
            if !exact_duplicate {
                return Err(BridgeError::Conflict);
            }
            let disposition = client
                .accept_terminal_duplicate(binding.authority.clone(), chunk, metadata)
                .await
                .map_err(|_| BridgeError::Protocol)?;
            self.record_primary_model_completion(&owner, chunk, received_at)?;
            self.ordinal_store
                .mark_model_call_provider_final(&owner.run_key, &owner.model_call_id)
                .map_err(|error| match error {
                    crate::store::AdapterStoreError::Conflict => BridgeError::Conflict,
                    crate::store::AdapterStoreError::Unavailable
                    | crate::store::AdapterStoreError::Corrupt => BridgeError::Unavailable,
                })?;
            return Ok(disposition);
        }
        let contiguous_new = sequence == Some(expected);
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
        let disposition = client
            .accept_chunk(chunk, metadata)
            .await
            .map_err(|_| BridgeError::Protocol)?;
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
            self.record_primary_model_completion(&owner, chunk, received_at)?;
            self.ordinal_store
                .mark_model_call_provider_final(&owner.run_key, &owner.model_call_id)
                .map_err(|error| match error {
                    crate::store::AdapterStoreError::Conflict => BridgeError::Conflict,
                    crate::store::AdapterStoreError::Unavailable
                    | crate::store::AdapterStoreError::Corrupt => BridgeError::Unavailable,
                })?;
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

    fn record_primary_model_completion(
        &self,
        owner: &ModelExchangeOwner,
        chunk: &ModelChunkMessage,
        received_at: &Instant,
    ) -> Result<(), BridgeError> {
        let usage = primary_model_usage(chunk)?;
        self.ordinal_store
            .record_performance_completion(
                &owner.run_key,
                PerformanceOperationKind::PrimaryModel,
                &owner.model_call_id,
                received_at,
                PerformanceOperationCompletion {
                    duration_millis: None,
                    input_tokens: usage.input,
                    cached_tokens: usage.cached,
                    output_tokens: usage.output,
                },
            )
            .map_err(model_frame_store_error)
    }

    fn validate_chunk_authority(
        &self,
        chunk: &ModelChunkMessage,
        binding: &ModelRunBinding,
        received_at: &Instant,
    ) -> Result<(), BridgeError> {
        // Validate the complete envelope before the crash-safe frame write.
        // The client repeats these checks when it delivers to Core, but doing
        // them here is required because this method intentionally retains the
        // frame first: a stale or cross-scoped chunk must not become durable
        // merely because it arrived with the next sequence number.
        if !canonical_instant(received_at)
            || received_at.0 < binding.authority.lease.issued_at.0
            || received_at.0 >= binding.authority.lease.expires_at.0
            || chunk.lease != binding.authority.lease
            || chunk.worker_session_id != binding.authority.worker_session_id
            || chunk.session_identity != binding.authority.session_identity
        {
            return Err(BridgeError::StaleAuthority);
        }
        self.authority
            .validate_current(&binding.authority, received_at)
            .map_err(|rejection| match rejection {
                ModelAuthorityRejection::ExpiredLease
                | ModelAuthorityRejection::StaleLease
                | ModelAuthorityRejection::Unavailable => BridgeError::StaleAuthority,
            })
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
        if chunk.lease != binding.authority.lease
            || chunk.worker_session_id != binding.authority.worker_session_id
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
            // A late Provider frame does not need to reopen a Core session;
            // the session alias is retained only to complete the binding
            // shape for authority checks.
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
                    .then_some(exchange.clone())
            })
            .collect::<Vec<_>>();
        for exchange in exchanges {
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

    pub(crate) fn discard_messages_for_thread(
        &self,
        thread_id: &CodexThreadId,
    ) -> Result<(), BridgeError> {
        self.queue.discard_for_thread(thread_id)
    }

    /// Looks up the exact authority installed for a Core thread.  The
    /// adapter uses this after the first child chunk to install the same
    /// lineage binding in the action gate before that child can dispatch a
    /// tool; callers never synthesize authority from a chunk alone.
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
        if let Some(binding) = binding {
            // Core's public thread id can be the registered Kernel session
            // UUID, while the Worker also keeps its own canonical lineage id.
            // Both must point at this binding; a valid session alias cannot
            // be paired with an unrelated thread id.
            if envelope.session_id == binding.kernel_session_id
                && (envelope.thread_id == binding.kernel_session_id
                    || envelope.thread_id == binding.canonical_thread_id.0)
            {
                return Ok(binding);
            }
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
        // A nested child can be the first model call observed after a Worker
        // restart.  Its parent alias may not have been rebuilt in the
        // process-local map yet, while the exact parent authority is already
        // durable.  Loading that authority preserves the same root lease and
        // lets `install_child_binding` re-establish the edge atomically.
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
                // The parent session id is not persisted in lineage; only its
                // authority is needed to derive this child. Keep a distinct
                // placeholder so the child binding remains keyed by the
                // request's exact session.
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
            // Child requests have their own Core session identity. Keep that
            // exact value as the action-gate/session alias; the canonical
            // thread id remains the durable lineage key.
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
    ) -> Result<ModelPortStream, ModelPortFailure> {
        let prepared = self.prepare_open_stream(&request)?;
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
                    model_call_id: exchange.model_call_id,
                },
            );
        let opened = self.client.lock().await.open(command).await;
        if opened.is_err() {
            let _ = self.sink.remove(&exchange.exchange);
            if let Ok(mut exchanges) = self.exchanges.lock() {
                exchanges.remove(&exchange.exchange.0);
            }
            return Err(model_failure(BridgeError::Protocol));
        }
        let stream = stream::unfold(receiver, |mut receiver| async move {
            receiver.recv().await.map(|item| (item, receiver))
        });
        Ok(Box::pin(stream) as ModelPortStream)
    }

    fn prepare_open_stream(
        &self,
        request: &ModelPortRequest,
    ) -> Result<OpenStreamPreparation, ModelPortFailure> {
        let envelope: KernelRequestEnvelope = serde_json::from_str(&request.payload_json)
            .map_err(|_| model_failure(BridgeError::InvalidPayload))?;
        if envelope.request_id != request.request_id
            || envelope.provider != self.expected_provider
            || !envelope.request.is_object()
        {
            return Err(model_failure(BridgeError::InvalidPayload));
        }
        let binding = self.binding_for(&envelope).map_err(model_failure)?;
        // Child bindings can be restored from the durable lineage table while
        // the Kernel is already processing the replayed model response.  Put
        // the exact binding in the action gate before Core can authorize a
        // tool, rather than waiting for the outer adapter poll loop.
        self.sync_action_binding(&binding).map_err(model_failure)?;
        let payload_bytes = request.payload_json.as_bytes().to_vec();
        let request_digest = model_call_digest(&envelope.request).map_err(model_failure)?;
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
        self.ordinal_store
            .record_performance_start(
                &binding.run_key,
                PerformanceOperationKind::PrimaryModel,
                &model_call_id,
                &observed_at,
            )
            .map_err(model_store_failure)?;
        let phase = self
            .ordinal_store
            .model_call_phase(&binding.run_key, &model_call_id)
            .map_err(model_store_failure)?;
        if matches!(
            phase,
            Some(ModelCallPhase::ProviderFinal | ModelCallPhase::CoreCommitted)
        ) {
            let frames = self.load_model_call_frames(&binding.run_key, &model_call_id)?;
            return Ok(OpenStreamPreparation::Replay(replay_model_call_frames(
                &frames,
                &binding,
                &exchange,
                &self.authority,
            )?));
        }
        // If the process stopped after the exact final frame was durably
        // retained but before the phase update, the frame itself is the
        // recovery witness. Promote it before replaying; this keeps the
        // provider from being opened a second time for the same call.
        if phase == Some(ModelCallPhase::InFlight) {
            let frames = self.load_model_call_frames(&binding.run_key, &model_call_id)?;
            if frames.last().is_some_and(|frame| frame.is_final) {
                self.ordinal_store
                    .mark_model_call_provider_final(&binding.run_key, &model_call_id)
                    .map_err(model_store_failure)?;
                return Ok(OpenStreamPreparation::Replay(replay_model_call_frames(
                    &frames,
                    &binding,
                    &exchange,
                    &self.authority,
                )?));
            }
        }
        Ok(OpenStreamPreparation::Open(Box::new(OpenStreamExchange {
            binding,
            payload_bytes,
            request_digest,
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
                if open.lease != exchange.binding.authority.lease
                    || open.worker_session_id != exchange.binding.authority.worker_session_id
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
                    authority: exchange.binding.authority.clone(),
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
}

impl ModelPort for KernelModelPort {
    fn stream(
        &self,
        request: ModelPortRequest,
    ) -> BoxFuture<'static, Result<ModelPortStream, ModelPortFailure>> {
        let bridge = Arc::clone(&self.bridge);
        Box::pin(async move { bridge.open_stream(request).await })
    }
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
    let envelope: KernelRequestEnvelope =
        serde_json::from_value(payload.clone()).map_err(|_| BridgeError::InvalidPayload)?;
    model_call_digest(&envelope.request)
}

fn same_binding(left: &ModelRunBinding, right: &ModelRunBinding) -> bool {
    left.run_key == right.run_key
        && left.canonical_thread_id == right.canonical_thread_id
        && left.kernel_session_id == right.kernel_session_id
        && left.authority == right.authority
}

fn common_authority(left: &ModelLeaseAuthority, right: &ModelLeaseAuthority) -> bool {
    left.lease == right.lease
        && left.worker_session_id == right.worker_session_id
        && left.session_identity.product_session_id == right.session_identity.product_session_id
        && left.session_identity.stage_run_id == right.session_identity.stage_run_id
        && left.session_identity.worker_session_id == right.session_identity.worker_session_id
}

/// Returns the parent Core thread carried by the canonical Responses
/// metadata.  A child request is accepted only when this edge resolves to an
/// already-bound parent; an arbitrary job id or matching lease is not enough
/// to mint a child authority.
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

fn primary_model_usage(chunk: &ModelChunkMessage) -> Result<PrimaryModelUsage, BridgeError> {
    let Some(payload) = &chunk.payload else {
        return Ok(PrimaryModelUsage::default());
    };
    let payload = decode_payload(payload)?;
    let usage = payload
        .get("tokenUsage")
        .or_else(|| payload.get("token_usage"));
    let Some(usage) = usage else {
        return Ok(PrimaryModelUsage::default());
    };
    let input = non_negative_metric(usage, "inputTokens", "input_tokens")?;
    let cached = non_negative_metric(usage, "cachedInputTokens", "cached_input_tokens")?;
    let output = non_negative_metric(usage, "outputTokens", "output_tokens")?;
    if cached > input {
        return Err(BridgeError::InvalidPayload);
    }
    Ok(PrimaryModelUsage {
        input: input.saturating_sub(cached),
        cached,
        output,
    })
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
    match authority.validate_current(&binding.authority, &binding.opened_at) {
        Ok(()) => {}
        Err(
            ModelAuthorityRejection::ExpiredLease
            | ModelAuthorityRejection::StaleLease
            | ModelAuthorityRejection::Unavailable,
        ) => {
            return Err(model_failure(BridgeError::StaleAuthority));
        }
    }
    if frames.is_empty() {
        return Err(model_failure(BridgeError::Unavailable));
    }
    let mut items = Vec::new();
    for (index, chunk) in frames.iter().enumerate() {
        let expected_sequence =
            i64::try_from(index + 1).map_err(|_| model_failure(BridgeError::InvalidPayload))?;
        if chunk.sequence.0 != expected_sequence
            || chunk.model_exchange_id != *exchange
            || chunk.lease != binding.authority.lease
            || chunk.worker_session_id != binding.authority.worker_session_id
            || chunk.session_identity != binding.authority.session_identity
            || (chunk.is_final && index + 1 != frames.len())
            || (chunk.error.is_some() && !chunk.is_final)
        {
            return Err(model_failure(BridgeError::Conflict));
        }
        if let Some(error) = &chunk.error {
            items.push(Err(ModelPortFailure::new(
                format!("{:?}", error.code),
                "Provider Gateway returned a terminal model error",
            )));
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

fn validate_chunk_for_retention(chunk: &ModelChunkMessage) -> Result<(), BridgeError> {
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
        String::from_utf8(bytes).map_err(|_| BridgeError::InvalidPayload)?;
    } else if !chunk.is_final && chunk.error.is_none() {
        return Err(BridgeError::InvalidPayload);
    }
    // The private ledger is replayed through the same typed protocol mapper
    // as Core.  Validate that mapping before writing a new frame so malformed
    // generated identities or stream metadata cannot survive a crash and be
    // surfaced later as a durable response.
    frame_from_message(&ExecutionPortMessage::ModelChunkMessage(chunk.clone()))
        .map_err(|_| BridgeError::InvalidPayload)?;
    Ok(())
}

fn canonical_instant(instant: &Instant) -> bool {
    let value = instant.0.as_bytes();
    value.len() == 24
        && value[4] == b'-'
        && value[7] == b'-'
        && value[10] == b'T'
        && value[13] == b':'
        && value[16] == b':'
        && value[19] == b'.'
        && value[23] == b'Z'
        && value.iter().enumerate().all(|(index, byte)| {
            matches!(index, 4 | 7 | 10 | 13 | 16 | 19 | 23) || byte.is_ascii_digit()
        })
        && number(value, 5, 7).is_some_and(|month| (1..=12).contains(&month))
        && number(value, 8, 10).is_some_and(|day| (1..=31).contains(&day))
        && number(value, 11, 13).is_some_and(|hour| hour <= 23)
        && number(value, 14, 16).is_some_and(|minute| minute <= 59)
        && number(value, 17, 19).is_some_and(|second| second <= 59)
}

fn number(value: &[u8], start: usize, end: usize) -> Option<u8> {
    value
        .get(start..end)?
        .iter()
        .try_fold(0_u8, |number, byte| {
            number.checked_mul(10)?.checked_add(byte - b'0')
        })
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
        ModelChunkDisposition, ModelChunkFingerprint, ModelCursorStore, ModelLeaseAuthoritySource,
        ModelTerminationReason,
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
                stage_run_id: None,
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
            }
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
        let usage = primary_model_usage(&chunk).expect("decode terminal usage");
        assert_eq!(usage.input, 85);
        assert_eq!(usage.cached, 35);
        assert_eq!(usage.output, 20);

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
        std::fs::remove_dir_all(root).expect("remove terminal duplicate store");
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
    ) -> (ModelOpenMessage, ModelOpenMessage) {
        let source = SharedAuthoritySource::default().with_store(store.clone());
        let bridge = Arc::new(ExecutionPortModelBridge::new(
            store.clone(),
            ExecutionOutbox::open(store.clone()).expect("open child model-call outbox"),
            route.clone(),
            "loopback".to_owned(),
            source,
        ));
        bridge
            .install_binding(root_binding.clone())
            .expect("install child model-call bridge root");
        let model_port = bridge.model_port();
        let (stream_a, stream_b) = tokio::join!(
            model_port.stream(request_a.clone()),
            model_port.stream(request_b.clone()),
        );
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

    #[tokio::test]
    async fn child_model_calls_bind_independent_exchanges_and_replay_exactly_after_restart() {
        let (root, store, root_binding, route, request_a, request_b) = child_model_fixture();
        let (primary_open, secondary_open) =
            open_parallel_child_calls(&store, &root_binding, &route, &request_a, &request_b).await;
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
        replay_final_child_calls(&store, &root_binding, &route).await;
        let report = store
            .performance_report("child-run", 60_000)
            .expect("child performance report after restart replay");
        assert_eq!(report.primary_model_call_count, 2);
        assert_eq!(report.primary_model_wait_ms, 120_000);
        assert_eq!(report.primary_model_input_tokens, 0);
        assert_eq!(report.primary_model_cached_tokens, 0);
        assert_eq!(report.primary_model_output_tokens, 0);
        std::fs::remove_dir_all(root).expect("remove child model-call store");
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
