// SPDX-License-Identifier: Apache-2.0

use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt;
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};

use futures::future::BoxFuture;
use sha2::{Digest, Sha256};
use tokio::sync::oneshot;
use winwincode_domain::{ExecutionMessageId, Instant, RequestId, Sha256Digest};
use winwincode_execution_port::{
    action_enforcement::{
        ActionEnforcementSigningKey, ActionEnforcementVerifier, ActionReceiptClaim,
        ActionReceiptUseStore, FileActionReceiptUseStore, prepare_action_enforcement_request,
    },
    action_gateway::{ExecutionEnvelopeToken, WorkerActionAuthority, WorkerActionRequest},
    action_normalizer::{
        ActionIntent, ActionPurpose, FileAnalysis, FileOperation, FileRequest, McpRequest,
        ObservedAction, ShellRequest, ToolRequest, canonical_mcp_capability_id, observe_action,
    },
    capability_adapter::WorkerCapabilityCatalog,
    generated::{ActionEnforcementDecision, ActionEnforcementReceiptMessage, ExecutionPortMessage},
};
use winwincode_kernel::{
    KernelActionAuthorization, KernelActionGate, KernelActionPayload, KernelActionRequest,
    KernelExecutableAuthorization, KernelFileOperation,
};

use crate::{ActionRequestTransport, model_bridge::ModelRunBinding};

#[derive(Clone)]
pub(crate) struct ExecutionPortActionGate {
    state: Arc<ActionGateState>,
}

struct ActionGateState {
    catalog: WorkerCapabilityCatalog,
    envelope: ExecutionEnvelopeToken,
    verifier: ActionEnforcementVerifier,
    receipt_store: Mutex<FileActionReceiptUseStore>,
    bindings: RwLock<HashMap<String, ModelRunBinding>>,
    read_only_runs: RwLock<HashMap<String, PathBuf>>,
    trusted_read_programs: HashMap<String, TrustedReadProgram>,
    session_generations: Mutex<HashMap<String, u64>>,
    trusted_now: Mutex<Option<Instant>>,
    pending: Mutex<HashMap<String, PendingAction>>,
    completed: Mutex<HashMap<String, CompletedAction>>,
    messages: Mutex<VecDeque<ExecutionPortMessage>>,
    request_transport: Mutex<Option<ActionRequestTransport>>,
}

struct PendingAction {
    action: WorkerActionRequest,
    session_id: String,
    generation: u64,
    response: oneshot::Sender<Result<(), ActionBridgeError>>,
}

struct ActionQueueContext<'a> {
    state: &'a Arc<ActionGateState>,
    binding: &'a ModelRunBinding,
    request: &'a KernelActionRequest,
    request_digest: &'a [u8],
    now: &'a Instant,
    generation: u64,
    request_ids: &'a mut Vec<RequestId>,
}

struct PreparedToolRequest {
    request: ToolRequest,
    observed: ObservedAction,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct TrustedReadProgram {
    canonical_path: PathBuf,
    identity: ExecutableIdentity,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ExecutableIdentity {
    device: u64,
    inode: u64,
    mode: u32,
    owner: u32,
    length: u64,
    modified_seconds: i64,
    modified_nanoseconds: i64,
    content_sha256: [u8; 32],
}

#[derive(Clone)]
struct CompletedAction {
    receipt_signature: Sha256Digest,
    permitted: bool,
    session_id: String,
    generation: u64,
}

impl ExecutionPortActionGate {
    pub(crate) fn open(
        worker_data_root: &Path,
        catalog: WorkerCapabilityCatalog,
        envelope: ExecutionEnvelopeToken,
        signing_key: ActionEnforcementSigningKey,
    ) -> Result<Self, ActionBridgeError> {
        let receipt_store = FileActionReceiptUseStore::open(worker_data_root)
            .map_err(|_| ActionBridgeError::Unavailable)?;
        Ok(Self {
            state: Arc::new(ActionGateState {
                catalog,
                envelope,
                verifier: signing_key.into_verifier(),
                receipt_store: Mutex::new(receipt_store),
                bindings: RwLock::new(HashMap::new()),
                read_only_runs: RwLock::new(HashMap::new()),
                trusted_read_programs: resolve_trusted_read_programs(),
                session_generations: Mutex::new(HashMap::new()),
                trusted_now: Mutex::new(None),
                pending: Mutex::new(HashMap::new()),
                completed: Mutex::new(HashMap::new()),
                messages: Mutex::new(VecDeque::new()),
                request_transport: Mutex::new(None),
            }),
        })
    }

    pub(crate) fn install_request_transport(&self, transport: ActionRequestTransport) {
        if let Ok(mut installed) = self.state.request_transport.lock() {
            *installed = Some(transport);
        }
    }

    pub(crate) fn install_binding(
        &self,
        binding: ModelRunBinding,
        read_only_workspace: Option<&Path>,
    ) -> Result<(), ActionBridgeError> {
        let run_key = binding.run_key.clone();
        self.install_binding_mode(binding, true)?;
        let mut read_only_runs = self
            .state
            .read_only_runs
            .write()
            .map_err(|_| ActionBridgeError::Unavailable)?;
        if let Some(workspace) = read_only_workspace {
            let workspace =
                std::fs::canonicalize(workspace).map_err(|_| ActionBridgeError::Unavailable)?;
            read_only_runs.insert(run_key, workspace);
        } else {
            read_only_runs.remove(&run_key);
        }
        Ok(())
    }

    /// Installs a descendant binding without allowing it to overwrite an
    /// existing session alias. Root lease replacement uses
    /// [`Self::install_binding`] and is the only operation allowed to fence a
    /// previous run's aliases.
    pub(crate) fn install_child_binding(
        &self,
        binding: ModelRunBinding,
    ) -> Result<(), ActionBridgeError> {
        self.install_binding_mode(binding, false)
    }

    fn install_binding_mode(
        &self,
        binding: ModelRunBinding,
        replace_fenced_run: bool,
    ) -> Result<(), ActionBridgeError> {
        let kernel_session_id = binding.kernel_session_id.clone();
        let thread_id = binding.canonical_thread_id.0.clone();
        let stale_session_ids = {
            let mut bindings = self
                .state
                .bindings
                .write()
                .map_err(|_| ActionBridgeError::Unavailable)?;
            // Re-installing the exact root authority is an idempotent
            // recovery operation, not a new fencing event.  Preserve its
            // already-discovered child aliases; only a changed root may
            // invalidate the complete run lineage.
            let fence_lineage = replace_fenced_run
                && !bindings
                    .get(&thread_id)
                    .is_some_and(|existing| same_binding(existing, &binding));
            for alias in [&kernel_session_id, &thread_id] {
                if bindings.get(alias).is_some_and(|existing| {
                    !same_binding(existing, &binding)
                        && (!fence_lineage
                            || existing.authority.lease.job_id != binding.authority.lease.job_id)
                }) {
                    return Err(ActionBridgeError::Conflict);
                }
            }
            let stale_session_ids = bindings
                .values()
                .filter(|existing| {
                    existing.authority.lease.job_id == binding.authority.lease.job_id
                        && !same_binding(existing, &binding)
                        && (!fence_lineage
                            || existing.run_key != binding.run_key
                            || existing.canonical_thread_id == binding.canonical_thread_id)
                })
                .flat_map(|existing| {
                    [
                        existing.kernel_session_id.clone(),
                        existing.canonical_thread_id.0.clone(),
                    ]
                })
                .collect::<HashSet<_>>();
            if fence_lineage {
                // A replacement root fences the complete run lineage.  Do not
                // preserve descendants merely because they share the run key:
                // their old lease must not resolve a tool request after the
                // replacement is installed.
                bindings.retain(|_, existing| {
                    existing.authority.lease.job_id != binding.authority.lease.job_id
                        || same_binding(existing, &binding)
                });
            } else {
                bindings.retain(|_, existing| {
                    existing.authority.lease.job_id != binding.authority.lease.job_id
                        || (existing.run_key == binding.run_key
                            && existing.canonical_thread_id != binding.canonical_thread_id)
                        || same_binding(existing, &binding)
                });
            }
            // Core tool-gate requests identify the live Core thread, while
            // model and runtime envelopes may use the registered session key.
            // Keep both aliases bound to the same immutable authority,
            // especially for spawned child threads.
            bindings.insert(binding.kernel_session_id.clone(), binding.clone());
            bindings.insert(binding.canonical_thread_id.0.clone(), binding);
            stale_session_ids
        };
        let mut generations = self
            .state
            .session_generations
            .lock()
            .map_err(|_| ActionBridgeError::Unavailable)?;
        generations.entry(kernel_session_id).or_insert(0);
        generations.entry(thread_id).or_insert(0);
        drop(generations);
        for session_id in stale_session_ids {
            self.cancel_session(&session_id)?;
        }
        Ok(())
    }

    pub(crate) fn remove_binding(
        &self,
        binding: &ModelRunBinding,
    ) -> Result<(), ActionBridgeError> {
        let session_ids = {
            let mut bindings = self
                .state
                .bindings
                .write()
                .map_err(|_| ActionBridgeError::Unavailable)?;
            let session_ids = bindings
                .values()
                .filter(|candidate| candidate.run_key == binding.run_key)
                .flat_map(|candidate| {
                    [
                        candidate.kernel_session_id.clone(),
                        candidate.canonical_thread_id.0.clone(),
                    ]
                })
                .collect::<HashSet<_>>();
            bindings.retain(|_, candidate| candidate.run_key != binding.run_key);
            session_ids
        };
        for session_id in session_ids {
            self.cancel_session(&session_id)?;
        }
        self.state
            .read_only_runs
            .write()
            .map_err(|_| ActionBridgeError::Unavailable)?
            .remove(&binding.run_key);
        Ok(())
    }

    pub(crate) fn update_now(&self, now: &Instant) -> Result<(), ActionBridgeError> {
        let mut trusted_now = self
            .state
            .trusted_now
            .lock()
            .map_err(|_| ActionBridgeError::Unavailable)?;
        // Control-plane responses can arrive out of order.  Treat the clock
        // as a high-water mark so an older receipt cannot move trusted time
        // backwards and reopen an expired lease or permit.
        if trusted_now
            .as_ref()
            .is_none_or(|current| current.0.as_str() < now.0.as_str())
        {
            *trusted_now = Some(now.clone());
        }
        Ok(())
    }

    pub(crate) fn accept_receipt(
        &self,
        receipt: &ActionEnforcementReceiptMessage,
        received_at: &Instant,
    ) -> Result<(), ActionBridgeError> {
        let Some((action, session_id, pending_generation)) =
            self.pending_receipt_action(receipt)?
        else {
            return self.completed_receipt_result(receipt);
        };
        self.validate_receipt(&action, &session_id, receipt, received_at)?;
        let (pending, generation) =
            self.take_pending_for_receipt(receipt, &session_id, pending_generation, received_at)?;
        if receipt.decision != ActionEnforcementDecision::Permit {
            let _ = pending.response.send(Err(ActionBridgeError::Rejected));
            self.remember_completed(receipt, false, &session_id, generation)?;
            return Ok(());
        }
        let claim = self
            .state
            .receipt_store
            .lock()
            .map_err(|_| ActionBridgeError::Unavailable)?
            .claim(receipt)
            .map_err(|_| ActionBridgeError::Unavailable)?;
        let outcome = match claim {
            ActionReceiptClaim::Fresh => Ok(()),
            ActionReceiptClaim::AlreadyConsumed => Err(ActionBridgeError::Consumed),
        };
        self.remember_completed(receipt, outcome.is_ok(), &session_id, generation)?;
        let _ = pending.response.send(outcome);
        outcome
    }

    fn pending_receipt_action(
        &self,
        receipt: &ActionEnforcementReceiptMessage,
    ) -> Result<Option<(WorkerActionRequest, String, u64)>, ActionBridgeError> {
        Ok(self
            .state
            .pending
            .lock()
            .map_err(|_| ActionBridgeError::Unavailable)?
            .get(&receipt.request_id.0)
            .map(|pending| {
                (
                    pending.action.clone(),
                    pending.session_id.clone(),
                    pending.generation,
                )
            }))
    }

    fn completed_receipt_result(
        &self,
        receipt: &ActionEnforcementReceiptMessage,
    ) -> Result<(), ActionBridgeError> {
        let completed = self
            .state
            .completed
            .lock()
            .map_err(|_| ActionBridgeError::Unavailable)?
            .get(&receipt.request_id.0)
            .cloned();
        match completed.as_ref() {
            Some(completed) if completed.receipt_signature == receipt.receipt_signature => {
                Err(ActionBridgeError::Consumed)
            }
            _ => Err(ActionBridgeError::UnknownReceipt),
        }
    }

    fn validate_receipt(
        &self,
        action: &WorkerActionRequest,
        session_id: &str,
        receipt: &ActionEnforcementReceiptMessage,
        received_at: &Instant,
    ) -> Result<(), ActionBridgeError> {
        let current_binding = self
            .state
            .bindings
            .read()
            .map_err(|_| ActionBridgeError::Unavailable)?
            .get(session_id)
            .cloned();
        let stale = current_binding.as_ref().is_none_or(|binding| {
            binding.authority.lease != action.authority.lease
                || binding.authority.worker_session_id != action.authority.worker_session_id
                || binding.authority.session_identity != action.authority.session_identity
                || !canonical_instant(received_at)
                || !canonical_instant(&binding.authority.lease.issued_at)
                || !canonical_instant(&binding.authority.lease.expires_at)
                || received_at.0 < binding.authority.lease.issued_at.0
                || received_at.0 >= binding.authority.lease.expires_at.0
        });
        if stale {
            reject_pending(
                &self.state,
                &receipt.request_id.0,
                ActionBridgeError::StaleAuthority,
            );
            return Err(ActionBridgeError::StaleAuthority);
        }
        if self.state.verifier.verify_outcome(action, receipt).is_err() {
            reject_pending(
                &self.state,
                &receipt.request_id.0,
                ActionBridgeError::Rejected,
            );
            return Err(ActionBridgeError::Rejected);
        }
        Ok(())
    }

    fn take_pending_for_receipt(
        &self,
        receipt: &ActionEnforcementReceiptMessage,
        session_id: &str,
        pending_generation: u64,
        received_at: &Instant,
    ) -> Result<(PendingAction, u64), ActionBridgeError> {
        // Keep the generation lock while taking the pending request.  The
        // cancellation path acquires these same locks in this order, so a
        // cancel racing with a receipt cannot observe the old generation and
        // then lose the race after the permit has been accepted.
        let mut generations = self
            .state
            .session_generations
            .lock()
            .map_err(|_| ActionBridgeError::Unavailable)?;
        let generation = *generations.entry(session_id.to_owned()).or_insert(0);
        if pending_generation != generation {
            drop(generations);
            reject_pending(
                &self.state,
                &receipt.request_id.0,
                ActionBridgeError::Cancelled,
            );
            return Err(ActionBridgeError::Cancelled);
        }
        self.update_now(received_at)?;
        let pending = self
            .state
            .pending
            .lock()
            .map_err(|_| ActionBridgeError::Unavailable)?
            .remove(&receipt.request_id.0)
            .ok_or(ActionBridgeError::Cancelled)?;
        drop(generations);
        Ok((pending, generation))
    }

    pub(crate) fn take_messages(&self) -> Result<Vec<ExecutionPortMessage>, ActionBridgeError> {
        Ok(self
            .state
            .messages
            .lock()
            .map_err(|_| ActionBridgeError::Unavailable)?
            .drain(..)
            .collect())
    }

    pub(crate) fn enqueue_message(
        &self,
        message: ExecutionPortMessage,
    ) -> Result<(), ActionBridgeError> {
        let mut messages = self
            .state
            .messages
            .lock()
            .map_err(|_| ActionBridgeError::Unavailable)?;
        if !messages.iter().any(|existing| existing == &message) {
            messages.push_back(message);
        }
        Ok(())
    }

    pub(crate) fn cancel_session(&self, session_id: &str) -> Result<(), ActionBridgeError> {
        let pending = {
            let mut generations = self
                .state
                .session_generations
                .lock()
                .map_err(|_| ActionBridgeError::Unavailable)?;
            let generation = generations.entry(session_id.to_owned()).or_insert(0);
            *generation = generation
                .checked_add(1)
                .ok_or(ActionBridgeError::Conflict)?;
            self.state
                .pending
                .lock()
                .map_err(|_| ActionBridgeError::Unavailable)?
                .extract_if(|_, pending| pending.session_id == session_id)
                .map(|(_, pending)| pending)
                .collect::<Vec<_>>()
        };
        for pending in pending {
            let _ = pending.response.send(Err(ActionBridgeError::Cancelled));
        }
        Ok(())
    }

    fn remember_completed(
        &self,
        receipt: &ActionEnforcementReceiptMessage,
        permitted: bool,
        session_id: &str,
        generation: u64,
    ) -> Result<(), ActionBridgeError> {
        self.state
            .completed
            .lock()
            .map_err(|_| ActionBridgeError::Unavailable)?
            .insert(
                receipt.request_id.0.clone(),
                CompletedAction {
                    receipt_signature: receipt.receipt_signature.clone(),
                    permitted,
                    session_id: session_id.to_owned(),
                    generation,
                },
            );
        Ok(())
    }
}

fn same_binding(left: &ModelRunBinding, right: &ModelRunBinding) -> bool {
    left.run_key == right.run_key
        && left.canonical_thread_id == right.canonical_thread_id
        && left.kernel_session_id == right.kernel_session_id
        && left.authority == right.authority
}

fn reject_pending(state: &ActionGateState, request_id: &str, error: ActionBridgeError) {
    let pending = state
        .pending
        .lock()
        .ok()
        .and_then(|mut pending| pending.remove(request_id));
    if let Some(pending) = pending {
        let _ = pending.response.send(Err(error));
    }
}

impl KernelActionGate for ExecutionPortActionGate {
    fn authorize(
        &self,
        request: KernelActionRequest,
    ) -> BoxFuture<'static, winwincode_kernel::KernelResult<KernelActionAuthorization>> {
        let state = Arc::clone(&self.state);
        Box::pin(async move {
            authorize_action(&state, request)
                .await
                .map_err(|_| winwincode_kernel::KernelFailure::action_rejected())
        })
    }

    fn revalidate(
        &self,
        request: KernelActionRequest,
        authorization: KernelActionAuthorization,
    ) -> BoxFuture<'static, winwincode_kernel::KernelResult<()>> {
        let state = Arc::clone(&self.state);
        Box::pin(async move {
            revalidate_action(&state, &request, &authorization)
                .map_err(|_| winwincode_kernel::KernelFailure::action_rejected())
        })
    }
}

async fn authorize_action(
    state: &Arc<ActionGateState>,
    request: KernelActionRequest,
) -> Result<KernelActionAuthorization, ActionBridgeError> {
    let (binding, now) = current_binding_and_time(state, &request.session_id)?;
    if let Some(authorization) = delegated_read_only_authorization(state, &binding, &request)? {
        return Ok(authorization);
    }
    let generation = current_generation(state, &request.session_id)?;
    let tool_requests = prepare_tool_requests(state, &binding, &request)?;
    let request_digest = kernel_request_digest(&request);
    let (request_ids, receivers) = {
        let mut receivers = Vec::with_capacity(tool_requests.len());
        let mut request_ids = Vec::with_capacity(tool_requests.len());
        let mut context = ActionQueueContext {
            state,
            binding: &binding,
            request: &request,
            request_digest: &request_digest,
            now: &now,
            generation,
            request_ids: &mut request_ids,
        };
        for (index, tool_request) in tool_requests.into_iter().enumerate() {
            let (request_id, receiver) = queue_action(&mut context, index, tool_request)?;
            receivers.push(receiver);
            context.request_ids.push(request_id);
        }
        (request_ids, receivers)
    };
    await_action_receivers(state, request_ids, receivers).await?;
    Ok(bound_authorization(&request, None))
}

fn prepare_tool_requests(
    state: &ActionGateState,
    binding: &ModelRunBinding,
    request: &KernelActionRequest,
) -> Result<Vec<PreparedToolRequest>, ActionBridgeError> {
    canonical_tool_requests(request)?
        .into_iter()
        .map(|tool_request| {
            authorize_capability(state, binding, &tool_request)?;
            let observed =
                observe_action(&tool_request).map_err(|_| ActionBridgeError::InvalidAction)?;
            Ok(PreparedToolRequest {
                request: tool_request,
                observed,
            })
        })
        .collect()
}

fn queue_action(
    context: &mut ActionQueueContext<'_>,
    index: usize,
    prepared: PreparedToolRequest,
) -> Result<(RequestId, oneshot::Receiver<Result<(), ActionBridgeError>>), ActionBridgeError> {
    let PreparedToolRequest { request, observed } = prepared;
    let object = *observed
        .objects
        .first()
        .ok_or(ActionBridgeError::InvalidAction)?;
    let intent = ActionIntent {
        object,
        operation: observed.operation,
        intent: ActionPurpose::Implement,
        scope: observed.scope,
        targets: observed.targets.clone(),
        requirement_refs: Vec::new(),
        plan_refs: Vec::new(),
        expected_effect: "execute the exact embedded Codex tool request".to_owned(),
        scope_delta: None,
        rollback: None,
        executor_risk: observed.minimum_risk,
    };
    let request_id = action_request_id(
        context.binding,
        context.request,
        context.request_digest,
        index,
    );
    reject_completed_duplicate(
        context.state,
        context.request,
        &request_id,
        context.generation,
    )?;
    let action = WorkerActionRequest {
        invocation_request_id: request_id.clone(),
        authority: WorkerActionAuthority {
            lease: context.binding.authority.lease.clone(),
            worker_session_id: context.binding.authority.worker_session_id.clone(),
            session_identity: context.binding.authority.session_identity.clone(),
            envelope: context.state.envelope.clone(),
        },
        intent,
        request,
    };
    let message_id = ExecutionMessageId(canonical_id(
        "xmsg",
        b"winwincode-kernel-action-message.v1",
        &[request_id.0.as_bytes()],
    ));
    let message = prepare_action_enforcement_request(message_id, context.now.clone(), &action)
        .map_err(|_| ActionBridgeError::InvalidAction)?;
    let (response, receiver) = oneshot::channel();
    if insert_pending(
        context.state,
        request_id.0.clone(),
        PendingAction {
            action,
            session_id: context.request.session_id.clone(),
            generation: context.generation,
            response,
        },
        context.generation,
    )? {
        cancel_requests(context.state, context.request_ids)?;
        return Err(ActionBridgeError::Conflict);
    }
    let execution_message = ExecutionPortMessage::ActionEnforcementRequestMessage(message);
    context
        .state
        .messages
        .lock()
        .map_err(|_| ActionBridgeError::Unavailable)?
        .push_back(execution_message.clone());
    deliver_direct_action_response(
        context.state,
        execution_message,
        &request_id,
        context.request_ids,
    )?;
    Ok((request_id, receiver))
}

fn reject_completed_duplicate(
    state: &ActionGateState,
    request: &KernelActionRequest,
    request_id: &RequestId,
    generation: u64,
) -> Result<(), ActionBridgeError> {
    let Some(completed) = state
        .completed
        .lock()
        .map_err(|_| ActionBridgeError::Unavailable)?
        .get(&request_id.0)
        .cloned()
    else {
        return Ok(());
    };
    if completed.session_id != request.session_id || completed.generation != generation {
        state
            .completed
            .lock()
            .map_err(|_| ActionBridgeError::Unavailable)?
            .remove(&request_id.0);
        Ok(())
    } else if completed.permitted {
        Err(ActionBridgeError::Consumed)
    } else {
        Err(ActionBridgeError::Rejected)
    }
}

fn deliver_direct_action_response(
    state: &Arc<ActionGateState>,
    execution_message: ExecutionPortMessage,
    request_id: &RequestId,
    request_ids: &[RequestId],
) -> Result<(), ActionBridgeError> {
    let Some(transport) = state
        .request_transport
        .lock()
        .map_err(|_| ActionBridgeError::Unavailable)?
        .clone()
    else {
        return Ok(());
    };
    let responses = transport(execution_message).map_err(|()| ActionBridgeError::Unavailable)?;
    for response in responses {
        let ExecutionPortMessage::ActionEnforcementReceiptMessage(receipt) = response else {
            continue;
        };
        if receipt.request_id != *request_id {
            continue;
        }
        ExecutionPortActionGate {
            state: Arc::clone(state),
        }
        .accept_receipt(&receipt, &receipt.evaluated_at)?;
        return Ok(());
    }
    cancel_requests(state, request_ids)?;
    Err(ActionBridgeError::Rejected)
}

async fn await_action_receivers(
    state: &Arc<ActionGateState>,
    request_ids: Vec<RequestId>,
    receivers: Vec<oneshot::Receiver<Result<(), ActionBridgeError>>>,
) -> Result<(), ActionBridgeError> {
    for receiver in receivers {
        if let Err(error) = receiver.await.map_err(|_| ActionBridgeError::Cancelled)? {
            cancel_requests(state, &request_ids)?;
            return Err(error);
        }
    }
    Ok(())
}

fn current_generation(state: &ActionGateState, session_id: &str) -> Result<u64, ActionBridgeError> {
    Ok(*state
        .session_generations
        .lock()
        .map_err(|_| ActionBridgeError::Unavailable)?
        .entry(session_id.to_owned())
        .or_insert(0))
}

fn insert_pending(
    state: &ActionGateState,
    request_id: String,
    pending: PendingAction,
    generation: u64,
) -> Result<bool, ActionBridgeError> {
    let mut generations = state
        .session_generations
        .lock()
        .map_err(|_| ActionBridgeError::Unavailable)?;
    let current = *generations.entry(pending.session_id.clone()).or_insert(0);
    if current != generation {
        let _ = pending.response.send(Err(ActionBridgeError::Cancelled));
        return Err(ActionBridgeError::Cancelled);
    }
    Ok(state
        .pending
        .lock()
        .map_err(|_| ActionBridgeError::Unavailable)?
        .insert(request_id, pending)
        .is_some())
}

fn current_binding_and_time(
    state: &ActionGateState,
    session_id: &str,
) -> Result<(ModelRunBinding, Instant), ActionBridgeError> {
    let binding = state
        .bindings
        .read()
        .map_err(|_| ActionBridgeError::Unavailable)?
        .get(session_id)
        .cloned()
        .ok_or(ActionBridgeError::StaleAuthority)?;
    let now = state
        .trusted_now
        .lock()
        .map_err(|_| ActionBridgeError::Unavailable)?
        .clone()
        .ok_or(ActionBridgeError::StaleAuthority)?;
    if !canonical_instant(&now)
        || !canonical_instant(&binding.authority.lease.issued_at)
        || !canonical_instant(&binding.authority.lease.expires_at)
        || binding.authority.lease.issued_at.0 >= binding.authority.lease.expires_at.0
        || now.0 < binding.authority.lease.issued_at.0
        || now.0 >= binding.authority.lease.expires_at.0
    {
        return Err(ActionBridgeError::StaleAuthority);
    }
    Ok((binding, now))
}

fn revalidate_action(
    state: &ActionGateState,
    request: &KernelActionRequest,
    authorization: &KernelActionAuthorization,
) -> Result<(), ActionBridgeError> {
    if authorization.request_binding() != request_binding(request) {
        return Err(ActionBridgeError::StaleAuthority);
    }
    let (binding, _) = current_binding_and_time(state, &request.session_id)?;
    if let Some(current) = delegated_read_only_authorization(state, &binding, request)? {
        if &current != authorization {
            return Err(ActionBridgeError::StaleAuthority);
        }
        return Ok(());
    }
    if authorization.executable().is_some() {
        return Err(ActionBridgeError::StaleAuthority);
    }
    let generation = current_generation(state, &request.session_id)?;
    let tool_requests = canonical_tool_requests(request)?;
    let digest = kernel_request_digest(request);
    let completed = state
        .completed
        .lock()
        .map_err(|_| ActionBridgeError::Unavailable)?;
    for (index, tool_request) in tool_requests.iter().enumerate() {
        authorize_capability(state, &binding, tool_request)?;
        let request_id = action_request_id(&binding, request, &digest, index);
        if completed.get(&request_id.0).is_none_or(|completed| {
            !completed.permitted
                || completed.session_id != request.session_id
                || completed.generation != generation
        }) {
            return Err(ActionBridgeError::StaleAuthority);
        }
    }
    Ok(())
}

fn delegated_read_only_authorization(
    state: &ActionGateState,
    binding: &ModelRunBinding,
    request: &KernelActionRequest,
) -> Result<Option<KernelActionAuthorization>, ActionBridgeError> {
    let workspace = state
        .read_only_runs
        .read()
        .map_err(|_| ActionBridgeError::Unavailable)?
        .get(&binding.run_key)
        .cloned();
    let Some(workspace) = workspace else {
        return Ok(None);
    };
    if is_request_user_input(request) {
        return Ok(Some(bound_authorization(request, None)));
    }
    if let Some(executable) = explicit_read_only_shell_authorization(state, request, &workspace) {
        return Ok(Some(bound_authorization(request, Some(executable))));
    }
    Err(ActionBridgeError::Rejected)
}

fn is_request_user_input(request: &KernelActionRequest) -> bool {
    request.tool_name == "request_user_input"
        && request
            .namespace
            .as_deref()
            .is_none_or(|namespace| namespace.is_empty() || namespace == "functions")
        && matches!(request.payload, KernelActionPayload::Function { .. })
}

fn explicit_read_only_shell_authorization(
    state: &ActionGateState,
    request: &KernelActionRequest,
    workspace: &Path,
) -> Option<KernelExecutableAuthorization> {
    let KernelActionPayload::Shell {
        program,
        args,
        working_directory,
    } = &request.payload
    else {
        return None;
    };
    if !matches!(request.tool_name.as_str(), "shell_command" | "exec_command")
        || request
            .namespace
            .as_deref()
            .is_some_and(|namespace| !namespace.is_empty() && namespace != "functions")
        || args.iter().any(|arg| shell_read_arg_is_unsafe(arg))
    {
        return None;
    }
    let requested_program = Path::new(program);
    let requested_cwd = Path::new(working_directory);
    if requested_program.is_absolute()
        && (requested_program.starts_with(workspace)
            || requested_program.starts_with(requested_cwd))
    {
        return None;
    }
    let (program, executable) = trusted_read_program(&state.trusted_read_programs, program)?;
    if !read_paths_stay_in_workspace(working_directory, args, workspace) {
        return None;
    }
    read_only_program_arguments(program, args).then(|| {
        KernelExecutableAuthorization::new(
            executable.canonical_path.to_string_lossy().into_owned(),
            args.clone(),
            executable_identity_string(&executable.identity),
        )
    })
}

fn read_only_program_arguments(program: &str, args: &[String]) -> bool {
    match program {
        "cat" | "head" | "tail" | "wc" | "ls" | "pwd" | "stat" => true,
        "grep" => !args.iter().any(|arg| {
            matches!(arg.as_str(), "-f" | "--file")
                || arg.starts_with("--file=")
                || (arg.starts_with("-f") && arg.len() > 2)
        }),
        "rg" => !args.iter().any(|arg| {
            arg == "--pre"
                || arg.starts_with("--pre=")
                || arg == "--pre-glob"
                || arg.starts_with("--pre-glob=")
                || arg == "--config"
                || arg.starts_with("--config=")
                || arg == "--hostname-bin"
                || arg.starts_with("--hostname-bin=")
                || matches!(arg.as_str(), "-z" | "--search-zip")
                || matches!(arg.as_str(), "-f" | "--file" | "--ignore-file")
                || arg.starts_with("--file=")
                || arg.starts_with("--ignore-file=")
                || (arg.starts_with("-f") && arg.len() > 2)
        }),
        _ => false,
    }
}

fn resolve_trusted_read_programs() -> HashMap<String, TrustedReadProgram> {
    const PROGRAMS: &[&str] = &[
        "cat", "head", "tail", "wc", "ls", "pwd", "stat", "grep", "rg",
    ];
    PROGRAMS
        .iter()
        .filter(|name| {
            **name != "rg"
                || std::env::var_os("RIPGREP_CONFIG_PATH").is_none_or(|value| value.is_empty())
        })
        .filter_map(|name| {
            protected_program_directories()
                .iter()
                .find_map(|directory| {
                    let candidate = directory.join(name);
                    let canonical_path = std::fs::canonicalize(candidate).ok()?;
                    protected_executable(&canonical_path).then_some(())?;
                    let identity = executable_identity(&canonical_path)?;
                    Some((
                        (*name).to_owned(),
                        TrustedReadProgram {
                            canonical_path,
                            identity,
                        },
                    ))
                })
        })
        .collect()
}

fn trusted_read_program<'a>(
    trusted: &'a HashMap<String, TrustedReadProgram>,
    program: &str,
) -> Option<(&'a str, &'a TrustedReadProgram)> {
    let path = Path::new(program);
    let selected = if path.components().count() == 1
        && matches!(path.components().next(), Some(Component::Normal(_)))
    {
        trusted.get_key_value(program)
    } else if path.is_absolute() {
        let canonical = std::fs::canonicalize(path).ok()?;
        trusted
            .iter()
            .find(|(_, fixed)| fixed.canonical_path == canonical)
    } else {
        None
    }?;
    if selected.0 == "rg"
        && std::env::var_os("RIPGREP_CONFIG_PATH").is_some_and(|value| !value.is_empty())
    {
        return None;
    }
    let current = executable_identity(&selected.1.canonical_path)?;
    (current == selected.1.identity && protected_executable(&selected.1.canonical_path))
        .then_some((selected.0.as_str(), selected.1))
}

#[cfg(target_os = "macos")]
fn protected_program_directories() -> [PathBuf; 4] {
    ["/usr/bin", "/bin", "/usr/sbin", "/sbin"].map(PathBuf::from)
}

#[cfg(target_os = "linux")]
fn protected_program_directories() -> [PathBuf; 2] {
    ["/usr/bin", "/bin"].map(PathBuf::from)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn protected_program_directories() -> [PathBuf; 0] {
    []
}

#[cfg(unix)]
fn protected_executable(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file()
        || metadata.uid() != 0
        || metadata.mode() & 0o111 == 0
        || metadata.mode() & 0o022 != 0
    {
        return false;
    }
    path.ancestors().skip(1).all(|ancestor| {
        std::fs::metadata(ancestor).is_ok_and(|metadata| {
            metadata.is_dir() && metadata.uid() == 0 && metadata.mode() & 0o022 == 0
        })
    })
}

#[cfg(not(unix))]
fn protected_executable(_path: &Path) -> bool {
    false
}

#[cfg(unix)]
fn executable_identity(path: &Path) -> Option<ExecutableIdentity> {
    let metadata = std::fs::metadata(path).ok()?;
    let bytes = std::fs::read(path).ok()?;
    Some(ExecutableIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
        mode: metadata.mode(),
        owner: metadata.uid(),
        length: metadata.len(),
        modified_seconds: metadata.mtime(),
        modified_nanoseconds: metadata.mtime_nsec(),
        content_sha256: Sha256::digest(bytes).into(),
    })
}

#[cfg(not(unix))]
fn executable_identity(_path: &Path) -> Option<ExecutableIdentity> {
    None
}

fn executable_identity_string(identity: &ExecutableIdentity) -> String {
    let mut digest = Sha256::new();
    digest.update(b"winwincode-delegated-read-executable.v1\0");
    digest.update(identity.device.to_be_bytes());
    digest.update(identity.inode.to_be_bytes());
    digest.update(identity.mode.to_be_bytes());
    digest.update(identity.owner.to_be_bytes());
    digest.update(identity.length.to_be_bytes());
    digest.update(identity.modified_seconds.to_be_bytes());
    digest.update(identity.modified_nanoseconds.to_be_bytes());
    digest.update(identity.content_sha256);
    format!("sha256:{}", hex_bytes(&digest.finalize()))
}

fn bound_authorization(
    request: &KernelActionRequest,
    executable: Option<KernelExecutableAuthorization>,
) -> KernelActionAuthorization {
    KernelActionAuthorization::new(request_binding(request), executable)
}

fn request_binding(request: &KernelActionRequest) -> String {
    format!("sha256:{}", hex_bytes(&kernel_request_digest(request)))
}

fn hex_bytes(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(char::from(HEX[usize::from(byte >> 4)]));
        encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    encoded
}

fn read_paths_stay_in_workspace(
    working_directory: &str,
    args: &[String],
    workspace: &Path,
) -> bool {
    let Ok(working_directory) = std::fs::canonicalize(working_directory) else {
        return false;
    };
    if !working_directory.starts_with(workspace) {
        return false;
    }
    args.iter().all(|arg| {
        [
            Some(arg.as_str()),
            arg.split_once('=').map(|(_, value)| value),
        ]
        .into_iter()
        .flatten()
        .all(|candidate| {
            let path = Path::new(candidate);
            if path.is_absolute()
                || path
                    .components()
                    .any(|component| component == Component::ParentDir)
            {
                return false;
            }
            let candidate = working_directory.join(path);
            !candidate.exists()
                || std::fs::canonicalize(candidate)
                    .is_ok_and(|canonical| canonical.starts_with(workspace))
        })
    })
}

fn shell_read_arg_is_unsafe(arg: &str) -> bool {
    matches!(arg, ">" | ">>" | "<" | "<<" | "|" | "||" | "&&" | ";")
        || arg.contains('\n')
        || arg.contains('\r')
}

fn authorize_capability(
    state: &ActionGateState,
    binding: &ModelRunBinding,
    tool_request: &ToolRequest,
) -> Result<(), ActionBridgeError> {
    if let ToolRequest::Mcp(mcp) = tool_request {
        let capability_id = canonical_mcp_capability_id(&mcp.server, &mcp.tool)
            .map_err(|_| ActionBridgeError::UnknownCapability)?;
        state
            .catalog
            .authorize(
                &capability_id,
                binding.authority.worker_session_id.clone(),
                state.envelope.clone(),
            )
            .map_err(|_| ActionBridgeError::UnknownCapability)?;
    }
    Ok(())
}

fn action_request_id(
    binding: &ModelRunBinding,
    request: &KernelActionRequest,
    request_digest: &[u8],
    index: usize,
) -> RequestId {
    RequestId(canonical_id(
        "req",
        b"winwincode-kernel-action-request.v2",
        &[
            binding.run_key.as_bytes(),
            request.operation_id.as_bytes(),
            request_digest,
            &(index as u64).to_be_bytes(),
        ],
    ))
}

fn cancel_requests(
    state: &ActionGateState,
    request_ids: &[RequestId],
) -> Result<(), ActionBridgeError> {
    let mut pending = state
        .pending
        .lock()
        .map_err(|_| ActionBridgeError::Unavailable)?;
    for request_id in request_ids {
        if let Some(pending) = pending.remove(&request_id.0) {
            let _ = pending.response.send(Err(ActionBridgeError::Cancelled));
        }
    }
    Ok(())
}

fn canonical_tool_requests(
    request: &KernelActionRequest,
) -> Result<Vec<ToolRequest>, ActionBridgeError> {
    const DEFAULT_FUNCTION_NAMESPACE: &str = "functions";
    // `request_user_input` is a built-in control tool.  It has no side effect
    // for the action gateway to authorize: the handler pauses the Kernel and
    // emits the durable InputRequest frame, then waits for the corresponding
    // InputResponse.  Keep the host identity/time checks in `authorize_action`
    // and `revalidate_action`, but do not turn this control interaction into
    // an ActionEnforcement request (which would reject it as an unknown
    // capability before the handler can emit the input request).
    if request.tool_name == "request_user_input"
        && request
            .namespace
            .as_deref()
            .is_none_or(|namespace| namespace.is_empty() || namespace == DEFAULT_FUNCTION_NAMESPACE)
    {
        return match request.payload {
            KernelActionPayload::Function { .. } => Ok(Vec::new()),
            _ => Err(ActionBridgeError::InvalidAction),
        };
    }
    if let Some(namespace) = request
        .namespace
        .as_deref()
        .filter(|namespace| !namespace.is_empty() && *namespace != DEFAULT_FUNCTION_NAMESPACE)
    {
        let arguments = match &request.payload {
            KernelActionPayload::Function { arguments } => arguments,
            KernelActionPayload::ToolSearch { arguments_json } => arguments_json,
            KernelActionPayload::Custom { input } => input,
            KernelActionPayload::Shell { .. } | KernelActionPayload::Files { .. } => {
                return Err(ActionBridgeError::InvalidAction);
            }
        };
        let value = serde_json::from_str(arguments)
            .unwrap_or_else(|_| serde_json::Value::String(arguments.clone()));
        return Ok(vec![ToolRequest::Mcp(McpRequest {
            server: namespace.to_owned(),
            tool: request.tool_name.clone(),
            arguments: value,
        })]);
    }
    match &request.payload {
        KernelActionPayload::Shell {
            program,
            args,
            working_directory,
        } if matches!(request.tool_name.as_str(), "shell_command" | "exec_command") => {
            if program.trim().is_empty() || working_directory.trim().is_empty() {
                return Err(ActionBridgeError::InvalidAction);
            }
            Ok(vec![ToolRequest::Shell(ShellRequest {
                program: program.clone(),
                args: args.clone(),
                working_directory: working_directory.clone(),
            })])
        }
        KernelActionPayload::Files { changes } if request.tool_name == "apply_patch" => {
            if changes.is_empty() {
                return Err(ActionBridgeError::InvalidAction);
            }
            let mut requests = Vec::new();
            for (kernel_operation, file_operation) in [
                (KernelFileOperation::Create, FileOperation::Create),
                (KernelFileOperation::Write, FileOperation::Write),
                (KernelFileOperation::Delete, FileOperation::Delete),
            ] {
                let mut paths = changes
                    .iter()
                    .filter(|change| change.operation == kernel_operation)
                    .flat_map(|change| {
                        std::iter::once(change.path.clone()).chain(change.move_path.clone())
                    })
                    .collect::<Vec<_>>();
                paths.sort();
                paths.dedup();
                if !paths.is_empty() {
                    requests.push(ToolRequest::File(FileRequest {
                        operation: file_operation,
                        paths,
                        analysis: FileAnalysis::default(),
                    }));
                }
            }
            Ok(requests)
        }
        _ => Err(ActionBridgeError::UnknownCapability),
    }
}

fn kernel_request_digest(request: &KernelActionRequest) -> Vec<u8> {
    let mut digest = Sha256::new();
    digest.update(b"winwincode-kernel-action-input.v1\0");
    for part in [
        request.session_id.as_str(),
        request.turn_id.as_str(),
        request.operation_id.as_str(),
        request.namespace.as_deref().unwrap_or(""),
        request.tool_name.as_str(),
    ] {
        digest.update((part.len() as u64).to_be_bytes());
        digest.update(part.as_bytes());
    }
    match &request.payload {
        KernelActionPayload::Function { arguments } => {
            digest.update(b"function\0");
            digest.update(arguments.as_bytes());
        }
        KernelActionPayload::ToolSearch { arguments_json } => {
            digest.update(b"tool_search\0");
            digest.update(arguments_json.as_bytes());
        }
        KernelActionPayload::Custom { input } => {
            digest.update(b"custom\0");
            digest.update(input.as_bytes());
        }
        KernelActionPayload::Shell {
            program,
            args,
            working_directory,
        } => {
            digest.update(b"shell\0");
            update_digest_part(&mut digest, program.as_bytes());
            update_digest_part(&mut digest, working_directory.as_bytes());
            digest.update((args.len() as u64).to_be_bytes());
            for arg in args {
                update_digest_part(&mut digest, arg.as_bytes());
            }
        }
        KernelActionPayload::Files { changes } => {
            digest.update(b"files\0");
            digest.update((changes.len() as u64).to_be_bytes());
            for change in changes {
                digest.update(match change.operation {
                    KernelFileOperation::Create => b"create\0".as_slice(),
                    KernelFileOperation::Write => b"write\0".as_slice(),
                    KernelFileOperation::Delete => b"delete\0".as_slice(),
                });
                update_digest_part(&mut digest, change.path.as_bytes());
                update_digest_part(
                    &mut digest,
                    change.move_path.as_deref().unwrap_or("").as_bytes(),
                );
            }
        }
    }
    digest.finalize().to_vec()
}

fn update_digest_part(digest: &mut Sha256, part: &[u8]) {
    digest.update((part.len() as u64).to_be_bytes());
    digest.update(part);
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

fn canonical_id(prefix: &str, namespace: &[u8], parts: &[&[u8]]) -> String {
    let mut digest = Sha256::new();
    digest.update(namespace);
    for part in parts {
        digest.update((part.len() as u64).to_be_bytes());
        digest.update(part);
    }
    let encoded = format!("{:x}", digest.finalize());
    format!("{prefix}_{}", &encoded[..26].to_ascii_uppercase())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ActionBridgeError {
    Unavailable,
    StaleAuthority,
    InvalidAction,
    UnknownCapability,
    UnknownReceipt,
    Rejected,
    Consumed,
    Conflict,
    Cancelled,
}

impl fmt::Display for ActionBridgeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("embedded Codex action enforcement failed")
    }
}

impl std::error::Error for ActionBridgeError {}

#[cfg(test)]
mod tests {
    use super::ExecutionPortActionGate;
    use crate::model_bridge::ModelRunBinding;
    use crate::model_port_client::ModelLeaseAuthority;
    use std::path::Path;
    use winwincode_domain::{
        CodexThreadId, ExecutionJobId, ExecutionMessageId, FencingToken, Instant, LeaseId,
        OrganizationId, ProductSessionId, ProjectId, RepositoryId, RepositoryScope,
        RepositoryScopeKind, SchemaVersion, SessionIdentity, Sha256Digest, UserActor,
        UserActorKind, UserId, WorkerId, WorkerInstanceId, WorkerSessionId, WorkspaceId,
    };
    use winwincode_execution_port::{
        action_enforcement::{
            ActionEnforcementIssuer, ActionEnforcementSigningKey, ActionReceiptUseStore as _,
        },
        action_gateway::ExecutionEnvelopeToken,
        action_normalizer::{
            ActionObject, ActionOperation, ActionRisk, ToolRequest, observe_action,
        },
        capability_adapter::WorkerCapabilityCatalog,
        generated::{
            ActionEnforcementDecision, ActionEnforcementReceiptMessage,
            ActionEnforcementReceiptMessageKind, ExecutionLeaseStamp, ExecutionPortMessage,
            WorkerCapabilityFeature, WorkerCapabilitySet, WorkerCapabilitySetPlatform,
        },
    };
    use winwincode_kernel::{
        KernelActionAuthorization, KernelActionGate as _, KernelActionPayload, KernelActionRequest,
        KernelFileChange, KernelFileOperation,
    };

    fn id(prefix: &str, value: char) -> String {
        format!("{prefix}_{}", value.to_string().repeat(26))
    }

    fn signing_key() -> ActionEnforcementSigningKey {
        ActionEnforcementSigningKey::from_bytes([41_u8; 32]).expect("signing key")
    }

    fn catalog() -> WorkerCapabilityCatalog {
        WorkerCapabilityCatalog::discover(
            &WorkerCapabilitySet {
                capability_digest: Sha256Digest(format!("sha256:{}", "a".repeat(64))),
                features: vec![WorkerCapabilityFeature::Sandbox],
                max_concurrent_jobs: 1,
                platform: WorkerCapabilitySetPlatform::Aarch64AppleDarwin,
            },
            Vec::new(),
        )
        .expect("capability catalog")
    }

    fn binding() -> ModelRunBinding {
        let worker_session_id = WorkerSessionId(id("wsn", 'A'));
        let canonical_thread_id = CodexThreadId(id("cdx", 'A'));
        ModelRunBinding {
            run_key: format!("sha256:{}", "b".repeat(64)),
            canonical_thread_id: canonical_thread_id.clone(),
            kernel_session_id: "kernel-session-action".to_owned(),
            authority: ModelLeaseAuthority {
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
                    codex_thread_id: canonical_thread_id,
                    product_session_id: ProductSessionId(id("psn", 'A')),
                    stage_run_id: None,
                    worker_session_id,
                },
            },
            opened_at: Instant("2030-01-01T00:00:00.000Z".to_owned()),
        }
    }

    fn shell_request(
        operation_id: &str,
        program: &str,
        args: &[&str],
        working_directory: &Path,
    ) -> KernelActionRequest {
        KernelActionRequest {
            session_id: "kernel-session-action".to_owned(),
            turn_id: "turn-read-only".to_owned(),
            operation_id: operation_id.to_owned(),
            namespace: Some("functions".to_owned()),
            tool_name: "exec_command".to_owned(),
            payload: KernelActionPayload::Shell {
                program: program.to_owned(),
                args: args.iter().map(|arg| (*arg).to_owned()).collect(),
                working_directory: working_directory.to_string_lossy().into_owned(),
            },
        }
    }

    fn patch_request(marker: &Path) -> KernelActionRequest {
        KernelActionRequest {
            session_id: "kernel-session-action".to_owned(),
            turn_id: "turn-read-only".to_owned(),
            operation_id: "call-patch".to_owned(),
            namespace: Some("functions".to_owned()),
            tool_name: "apply_patch".to_owned(),
            payload: KernelActionPayload::Files {
                changes: vec![KernelFileChange {
                    operation: KernelFileOperation::Write,
                    path: marker.to_string_lossy().into_owned(),
                    move_path: None,
                }],
            },
        }
    }

    fn assert_pending_shell_request(
        gate: &ExecutionPortActionGate,
        request: &winwincode_execution_port::generated::ActionEnforcementRequestMessage,
    ) {
        let pending = gate.state.pending.lock().expect("pending action");
        let exact = pending
            .get(&request.request_id.0)
            .expect("pending exact builtin action");
        assert!(matches!(
            &exact.action.request,
            ToolRequest::Shell(shell)
                if shell.program == "pnpm"
                    && shell.args == ["add", "exact-package"]
                    && shell.working_directory == "/tmp/exact"
        ));
        let observed = observe_action(&exact.action.request).expect("observe exact argv");
        assert_eq!(observed.objects, [ActionObject::Dependency]);
        assert_eq!(observed.operation, ActionOperation::Modify);
    }

    fn signed_permit(
        request: winwincode_execution_port::generated::ActionEnforcementRequestMessage,
        now: &Instant,
    ) -> ActionEnforcementReceiptMessage {
        let mut receipt = ActionEnforcementReceiptMessage {
            actor: UserActor {
                id: UserId(id("usr", 'A')),
                kind: UserActorKind::User,
            },
            decision: ActionEnforcementDecision::Permit,
            evaluated_at: now.clone(),
            evaluation_sha256: Sha256Digest(format!("sha256:{}", "d".repeat(64))),
            job_id: request.job_id,
            kind: ActionEnforcementReceiptMessageKind::ActionEnforcementReceipt,
            lease: request.lease,
            matched_condition_sha256: request.matched_condition_sha256,
            message_id: ExecutionMessageId(id("xmsg", 'A')),
            policy_kind: request.policy_kind,
            policy_mode: None,
            policy_version: None,
            receipt_signature: Sha256Digest(format!("sha256:{}", "0".repeat(64))),
            request_id: request.request_id,
            resource: request.resource,
            schema_version: SchemaVersion::WinwincodeV1,
            scope: RepositoryScope {
                kind: RepositoryScopeKind::Repository,
                organization_id: OrganizationId(id("org", 'A')),
                workspace_id: WorkspaceId(id("wsp", 'A')),
                project_id: ProjectId(id("prj", 'A')),
                repository_id: RepositoryId(id("rep", 'A')),
            },
            sent_at: now.clone(),
            session_identity: request.session_identity,
            subject_sha256: request.subject_sha256,
            worker_session_id: request.worker_session_id,
        };
        ActionEnforcementIssuer::new(signing_key())
            .sign(&mut receipt)
            .expect("sign permit");
        receipt
    }

    async fn assert_consumed_action_is_fenced(
        gate: &ExecutionPortActionGate,
        action_request: KernelActionRequest,
        authorization: KernelActionAuthorization,
        receipt: &ActionEnforcementReceiptMessage,
        now: &Instant,
    ) {
        gate.revalidate(action_request.clone(), authorization.clone())
            .await
            .expect("current permit revalidates immediately before runtime");
        assert_eq!(
            gate.authorize(action_request.clone()).await,
            Err(winwincode_kernel::KernelFailure::action_rejected())
        );
        assert!(
            gate.take_messages()
                .expect("duplicate action emits no new request")
                .is_empty()
        );
        let mut forged = receipt.clone();
        forged.receipt_signature = Sha256Digest(format!("sha256:{}", "1".repeat(64)));
        assert_eq!(
            gate.accept_receipt(&forged, now),
            Err(super::ActionBridgeError::UnknownReceipt)
        );
        assert_eq!(
            gate.accept_receipt(receipt, now),
            Err(super::ActionBridgeError::Consumed)
        );
        gate.cancel_session("kernel-session-action")
            .expect("cancel exact session generation");
        assert!(
            gate.revalidate(action_request.clone(), authorization.clone())
                .await
                .is_err(),
            "a cancelled permit cannot reach the side-effect seam"
        );

        let unknown = gate
            .authorize(KernelActionRequest {
                session_id: "kernel-session-action".to_owned(),
                turn_id: "turn-action".to_owned(),
                operation_id: "call-unknown".to_owned(),
                namespace: Some("missing-server".to_owned()),
                tool_name: "missing-tool".to_owned(),
                payload: KernelActionPayload::Custom {
                    input: "PAYLOAD".to_owned(),
                },
            })
            .await;
        assert!(unknown.is_err());
        assert!(gate.take_messages().expect("no denied message").is_empty());
        gate.update_now(&Instant("2030-01-01T01:00:00.000Z".to_owned()))
            .expect("advance trusted time to lease boundary");
        assert!(
            gate.revalidate(action_request.clone(), authorization.clone())
                .await
                .is_err()
        );
        gate.update_now(now)
            .expect("ignore an out-of-order older trusted time");
        assert_eq!(
            gate.state
                .trusted_now
                .lock()
                .expect("trusted action time")
                .as_ref(),
            Some(&Instant("2030-01-01T01:00:00.000Z".to_owned()))
        );
        gate.remove_binding(&binding())
            .expect("cancel exact binding");
        assert!(
            gate.revalidate(action_request, authorization)
                .await
                .is_err()
        );
    }

    fn delegated_rejected_requests(
        root: &Path,
        outside: &Path,
        marker: &Path,
    ) -> Vec<KernelActionRequest> {
        let mut requests = vec![
            shell_request(
                "call-shell",
                "sh",
                &["-c", "printf changed > marker.txt"],
                root,
            ),
            patch_request(marker),
            shell_request("call-find-delete", "find", &[".", "-delete"], root),
            shell_request("call-tree-write", "tree", &["-o", "tree.txt"], root),
            shell_request("call-codegen", "python3", &["generate.py"], root),
            shell_request("call-external-read", "cat", &["/etc/passwd"], root),
            shell_request("call-outside-cwd", "cat", &["secret.txt"], outside),
            shell_request("call-rg-pre", "rg", &["--pre=sh", "fixture"], root),
            shell_request(
                "call-rg-config",
                "rg",
                &["--config=rg.conf", "fixture"],
                root,
            ),
            shell_request(
                "call-parent-read",
                "grep",
                &["fixture", "../outside.txt"],
                root,
            ),
            shell_request(
                "call-git-pager",
                "git",
                &["grep", "--open-files-in-pager=sh", "fixture"],
                root,
            ),
            shell_request(
                "call-git-write",
                "git",
                &["checkout", "--", "marker.txt"],
                root,
            ),
        ];
        #[cfg(unix)]
        {
            requests.push(shell_request(
                "call-symlink-read",
                "cat",
                &["outside-link/secret.txt"],
                root,
            ));
            requests.push(shell_request(
                "call-candidate-executable",
                &root.join("candidate-cat").to_string_lossy(),
                &["marker.txt"],
                root,
            ));
        }
        requests
    }

    async fn assert_delegated_requests_rejected(
        gate: &ExecutionPortActionGate,
        requests: Vec<KernelActionRequest>,
    ) {
        for request in requests {
            assert_eq!(
                gate.authorize(request.clone()).await,
                Err(winwincode_kernel::KernelFailure::action_rejected())
            );
            assert_eq!(
                gate.revalidate(request, KernelActionAuthorization::default())
                    .await,
                Err(winwincode_kernel::KernelFailure::action_rejected())
            );
        }
    }

    #[tokio::test]
    async fn kernel_action_round_trips_through_signed_execution_port_receipt() {
        let root =
            std::env::temp_dir().join(format!("winwincode-codex-action-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let gate = ExecutionPortActionGate::open(
            &root,
            catalog(),
            ExecutionEnvelopeToken {
                version: 1,
                digest: Sha256Digest(format!("sha256:{}", "c".repeat(64))),
            },
            signing_key(),
        )
        .expect("open action gate");
        gate.install_binding(binding(), None)
            .expect("install binding");
        let now = Instant("2030-01-01T00:00:02.000Z".to_owned());
        gate.update_now(&now).expect("install trusted time");

        let action_request = KernelActionRequest {
            session_id: "kernel-session-action".to_owned(),
            turn_id: "turn-action".to_owned(),
            operation_id: "call-action".to_owned(),
            namespace: Some("functions".to_owned()),
            tool_name: "shell_command".to_owned(),
            payload: KernelActionPayload::Shell {
                program: "pnpm".to_owned(),
                args: vec!["add".to_owned(), "exact-package".to_owned()],
                working_directory: "/tmp/exact".to_owned(),
            },
        };
        let authorize = gate.authorize(action_request.clone());
        let task = tokio::spawn(authorize);
        tokio::task::yield_now().await;
        let mut messages = gate.take_messages().expect("action messages");
        let ExecutionPortMessage::ActionEnforcementRequestMessage(request) =
            messages.pop().expect("one action enforcement request")
        else {
            panic!("Kernel action must use the ActionEnforcement ExecutionPort message");
        };
        assert!(messages.is_empty());
        assert_pending_shell_request(&gate, &request);
        let receipt = signed_permit(request, &now);
        gate.accept_receipt(&receipt, &now)
            .expect("accept signed permit");
        let authorization = task
            .await
            .expect("Kernel authorization task")
            .expect("Kernel action authorized");
        assert_consumed_action_is_fenced(&gate, action_request, authorization, &receipt, &now)
            .await;
        std::fs::remove_dir_all(root).expect("remove action fixture");
    }

    #[tokio::test]
    async fn delegated_composer_allows_only_explicit_reads_before_the_action_queue() {
        let root = std::env::temp_dir().join(format!(
            "winwincode-codex-action-read-only-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("create read-only action fixture");
        let marker = root.join("marker.txt");
        std::fs::write(&marker, "unchanged").expect("write marker");
        let outside = root.with_extension("outside");
        let _ = std::fs::remove_dir_all(&outside);
        std::fs::create_dir_all(&outside).expect("create outside read fixture");
        std::fs::write(outside.join("secret.txt"), "outside").expect("write outside read fixture");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, root.join("outside-link"))
            .expect("create outside symlink fixture");
        #[cfg(unix)]
        std::os::unix::fs::symlink("/bin/cat", root.join("candidate-cat"))
            .expect("create candidate executable symlink");
        let gate = ExecutionPortActionGate::open(
            &root,
            catalog(),
            ExecutionEnvelopeToken {
                version: 1,
                digest: Sha256Digest(format!("sha256:{}", "c".repeat(64))),
            },
            signing_key(),
        )
        .expect("open action gate");
        gate.install_binding(binding(), Some(&root))
            .expect("install delegated binding");
        gate.update_now(&Instant("2030-01-01T00:00:02.000Z".to_owned()))
            .expect("install trusted time");

        let read = shell_request("call-read", "cat", &["marker.txt"], &root);
        let authorization = gate
            .authorize(read.clone())
            .await
            .expect("allow explicit repository search");
        let executable = authorization
            .executable()
            .expect("delegated read binds one executable");
        assert!(Path::new(executable.canonical_absolute_path()).is_absolute());
        assert!(executable.identity().starts_with("sha256:"));
        assert_eq!(executable.arguments(), ["marker.txt"]);
        gate.revalidate(read, authorization)
            .await
            .expect("revalidate explicit repository search");

        #[cfg(unix)]
        {
            let late = shell_request("call-late-link", "cat", &["late-link"], &root);
            let authorization = gate
                .authorize(late.clone())
                .await
                .expect("admit path before it exists");
            std::os::unix::fs::symlink(&outside, root.join("late-link"))
                .expect("replace read path with outside symlink");
            assert!(
                gate.revalidate(late, authorization).await.is_err(),
                "the final pre-execution check must reject a changed path"
            );
        }

        assert_delegated_requests_rejected(
            &gate,
            delegated_rejected_requests(&root, &outside, &marker),
        )
        .await;
        assert!(gate.take_messages().expect("no queued actions").is_empty());
        assert!(
            gate.state
                .pending
                .lock()
                .expect("no pending actions")
                .is_empty()
        );
        assert_eq!(
            std::fs::read_to_string(&marker).expect("read unchanged marker"),
            "unchanged"
        );
        std::fs::remove_dir_all(root).expect("remove read-only action fixture");
        std::fs::remove_dir_all(outside).expect("remove outside read fixture");
    }

    #[test]
    fn delegated_shell_read_allowlist_is_closed_against_execution_and_codegen() {
        for (program, args) in [
            ("cat", &["src/lib.rs"][..]),
            ("rg", &["fixture", "src"][..]),
            ("grep", &["fixture", "src/lib.rs"][..]),
        ] {
            assert!(super::read_only_program_arguments(
                program,
                &args.iter().map(ToString::to_string).collect::<Vec<_>>()
            ));
        }
        for (program, args) in [
            ("sh", &["-c", "cat src/lib.rs"][..]),
            ("sed", &["-i", "s/a/b/", "src/lib.rs"][..]),
            ("cargo", &["build"][..]),
            ("python3", &["generate.py"][..]),
            ("rg", &["--pre=tool", "fixture"][..]),
            ("rg", &["--pre-glob=*.rs", "fixture"][..]),
            ("rg", &["--config=rg.conf", "fixture"][..]),
            ("rg", &["--search-zip", "fixture"][..]),
            ("rg", &["-z", "fixture"][..]),
            ("rg", &["--hostname-bin=sh", "fixture"][..]),
            ("find", &[".", "-exec", "rm", "{}", ";"][..]),
            ("find", &[".", "-delete"][..]),
            ("find", &[".", "-fprint", "paths.txt"][..]),
            ("find", &[".", "-fprintf", "paths.txt", "%p"][..]),
            ("find", &[".", "-fprint0", "paths.bin"][..]),
            ("find", &[".", "-fls", "paths.txt"][..]),
            ("tree", &["-o", "tree.txt"][..]),
            ("git", &["grep", "--open-files-in-pager=sh", "fixture"][..]),
        ] {
            assert!(!super::read_only_program_arguments(
                program,
                &args.iter().map(ToString::to_string).collect::<Vec<_>>()
            ));
        }
        let trusted = super::resolve_trusted_read_programs();
        let (_, cat) =
            super::trusted_read_program(&trusted, "cat").expect("platform system cat is protected");
        assert!(cat.canonical_path.is_absolute());
        assert_eq!(cat.identity.owner, 0);
        assert!(cat.identity.mode & 0o111 != 0);
        assert!(super::trusted_read_program(&trusted, "./cat").is_none());
        assert!(super::trusted_read_program(&trusted, "/tmp/malicious/cat").is_none());
    }

    #[cfg(unix)]
    #[test]
    fn executable_identity_detects_symlink_and_in_place_replacement() {
        let root = std::env::temp_dir().join(format!(
            "winwincode-executable-identity-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("create executable identity fixture");
        let first = root.join("first");
        let second = root.join("second");
        let link = root.join("program");
        std::fs::write(&first, b"first").expect("write first executable");
        std::fs::write(&second, b"second").expect("write second executable");
        std::os::unix::fs::symlink(&first, &link).expect("link first executable");
        let first_identity = super::executable_identity(&link).expect("first identity");
        std::fs::remove_file(&link).expect("remove first link");
        std::os::unix::fs::symlink(&second, &link).expect("link second executable");
        let second_identity = super::executable_identity(&link).expect("second identity");
        assert_ne!(first_identity, second_identity);
        std::fs::write(&second, b"changed in place").expect("replace executable bytes");
        assert_ne!(
            second_identity,
            super::executable_identity(&link).expect("changed identity")
        );
        std::fs::remove_dir_all(root).expect("remove executable identity fixture");
    }

    #[tokio::test]
    async fn replacing_kernel_binding_cancels_pending_action_generation() {
        let root = std::env::temp_dir().join(format!(
            "winwincode-codex-action-rebind-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let gate = ExecutionPortActionGate::open(
            &root,
            catalog(),
            ExecutionEnvelopeToken {
                version: 1,
                digest: Sha256Digest(format!("sha256:{}", "c".repeat(64))),
            },
            signing_key(),
        )
        .expect("open action gate");
        let original = binding();
        gate.install_binding(original.clone(), None)
            .expect("install binding");
        let now = Instant("2030-01-01T00:00:02.000Z".to_owned());
        gate.update_now(&now).expect("install trusted time");
        let request = KernelActionRequest {
            session_id: original.kernel_session_id.clone(),
            turn_id: "turn-rebind".to_owned(),
            operation_id: "call-rebind".to_owned(),
            namespace: Some("functions".to_owned()),
            tool_name: "exec_command".to_owned(),
            payload: KernelActionPayload::Shell {
                program: "cargo".to_owned(),
                args: vec!["test".to_owned()],
                working_directory: "/tmp/rebind".to_owned(),
            },
        };
        let task = tokio::spawn(gate.authorize(request.clone()));
        tokio::task::yield_now().await;
        assert_eq!(gate.state.pending.lock().expect("pending action").len(), 1);

        let mut child = original.clone();
        child.canonical_thread_id = CodexThreadId(id("cdx", 'B'));
        child.kernel_session_id = "kernel-session-child".to_owned();
        child.authority.session_identity.codex_thread_id = child.canonical_thread_id.clone();
        gate.install_child_binding(child.clone())
            .expect("install descendant binding");
        gate.install_binding(original.clone(), None)
            .expect("idempotent root rebind preserves descendant");
        assert!(
            gate.state
                .bindings
                .read()
                .expect("bindings")
                .contains_key(&child.kernel_session_id)
        );

        let mut replacement = original.clone();
        replacement.kernel_session_id = "kernel-session-rebound".to_owned();
        gate.install_binding(replacement.clone(), None)
            .expect("replace binding");
        assert!(task.await.expect("authorization task").is_err());
        assert!(
            gate.state
                .pending
                .lock()
                .expect("pending action")
                .is_empty()
        );
        assert!(
            gate.state
                .bindings
                .read()
                .expect("bindings")
                .get(&original.kernel_session_id)
                .is_none()
        );
        {
            let bindings = gate.state.bindings.read().expect("bindings");
            assert!(bindings.get(&child.kernel_session_id).is_none());
            assert!(bindings.get(&child.canonical_thread_id.0).is_none());
        }
        assert!(
            gate.revalidate(request, KernelActionAuthorization::default())
                .await
                .is_err(),
            "replaced binding cannot reuse an old permit"
        );
        std::fs::remove_dir_all(root).expect("remove action fixture");
    }

    #[tokio::test]
    async fn expired_signed_permit_is_rejected_before_durable_claim() {
        let root = std::env::temp_dir().join(format!(
            "winwincode-codex-action-expired-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let gate = ExecutionPortActionGate::open(
            &root,
            catalog(),
            ExecutionEnvelopeToken {
                version: 1,
                digest: Sha256Digest(format!("sha256:{}", "c".repeat(64))),
            },
            signing_key(),
        )
        .expect("open action gate");
        gate.install_binding(binding(), None)
            .expect("install binding");
        let issued = Instant("2030-01-01T00:00:02.000Z".to_owned());
        gate.update_now(&issued).expect("install trusted time");
        let task = tokio::spawn(gate.authorize(KernelActionRequest {
            session_id: "kernel-session-action".to_owned(),
            turn_id: "turn-action".to_owned(),
            operation_id: "call-expired".to_owned(),
            namespace: Some("functions".to_owned()),
            tool_name: "shell_command".to_owned(),
            payload: KernelActionPayload::Shell {
                program: "cargo".to_owned(),
                args: vec!["test".to_owned()],
                working_directory: "/tmp/exact".to_owned(),
            },
        }));
        tokio::task::yield_now().await;
        let ExecutionPortMessage::ActionEnforcementRequestMessage(request) = gate
            .take_messages()
            .expect("take request")
            .pop()
            .expect("one request")
        else {
            panic!("expected action enforcement request");
        };
        let mut receipt = ActionEnforcementReceiptMessage {
            actor: UserActor {
                id: UserId(id("usr", 'A')),
                kind: UserActorKind::User,
            },
            decision: ActionEnforcementDecision::Permit,
            evaluated_at: issued.clone(),
            evaluation_sha256: Sha256Digest(format!("sha256:{}", "d".repeat(64))),
            job_id: request.job_id,
            kind: ActionEnforcementReceiptMessageKind::ActionEnforcementReceipt,
            lease: request.lease,
            matched_condition_sha256: request.matched_condition_sha256,
            message_id: ExecutionMessageId(id("xmsg", 'B')),
            policy_kind: request.policy_kind,
            policy_mode: None,
            policy_version: None,
            receipt_signature: Sha256Digest(format!("sha256:{}", "0".repeat(64))),
            request_id: request.request_id,
            resource: request.resource,
            schema_version: SchemaVersion::WinwincodeV1,
            scope: RepositoryScope {
                kind: RepositoryScopeKind::Repository,
                organization_id: OrganizationId(id("org", 'A')),
                workspace_id: WorkspaceId(id("wsp", 'A')),
                project_id: ProjectId(id("prj", 'A')),
                repository_id: RepositoryId(id("rep", 'A')),
            },
            sent_at: issued,
            session_identity: request.session_identity,
            subject_sha256: request.subject_sha256,
            worker_session_id: request.worker_session_id,
        };
        ActionEnforcementIssuer::new(signing_key())
            .sign(&mut receipt)
            .expect("sign permit");
        let expired = Instant("2030-01-01T01:00:00.000Z".to_owned());
        assert!(gate.accept_receipt(&receipt, &expired).is_err());
        assert!(task.await.expect("authorization task").is_err());
        assert_eq!(
            gate.state
                .receipt_store
                .lock()
                .expect("receipt store")
                .claim(&receipt)
                .expect("receipt remains unclaimed"),
            winwincode_execution_port::action_enforcement::ActionReceiptClaim::Fresh
        );
        std::fs::remove_dir_all(root).expect("remove action fixture");
    }

    #[test]
    fn typed_shell_and_mixed_patch_preserve_actual_runtime_facts() {
        let shell = super::canonical_tool_requests(&KernelActionRequest {
            session_id: "session".to_owned(),
            turn_id: "turn".to_owned(),
            operation_id: "call-shell".to_owned(),
            namespace: Some("functions".to_owned()),
            tool_name: "exec_command".to_owned(),
            payload: KernelActionPayload::Shell {
                program: "cargo".to_owned(),
                args: vec!["test".to_owned(), "--locked".to_owned()],
                working_directory: "/workspace".to_owned(),
            },
        })
        .expect("typed shell request");
        let observed = observe_action(&shell[0]).expect("observe cargo test");
        assert_eq!(observed.objects, [ActionObject::Test]);
        assert_eq!(observed.operation, ActionOperation::Execute);

        let destructive = super::canonical_tool_requests(&KernelActionRequest {
            session_id: "session".to_owned(),
            turn_id: "turn".to_owned(),
            operation_id: "call-delete".to_owned(),
            namespace: Some("functions".to_owned()),
            tool_name: "exec_command".to_owned(),
            payload: KernelActionPayload::Shell {
                program: "rm".to_owned(),
                args: vec!["-rf".to_owned(), "target".to_owned()],
                working_directory: "/workspace".to_owned(),
            },
        })
        .expect("typed destructive request");
        let observed = observe_action(&destructive[0]).expect("observe destructive argv");
        assert_eq!(observed.operation, ActionOperation::Delete);
        assert_eq!(observed.minimum_risk, ActionRisk::High);

        let files = super::canonical_tool_requests(&KernelActionRequest {
            session_id: "session".to_owned(),
            turn_id: "turn".to_owned(),
            operation_id: "call-patch".to_owned(),
            namespace: Some("functions".to_owned()),
            tool_name: "apply_patch".to_owned(),
            payload: KernelActionPayload::Files {
                changes: vec![
                    KernelFileChange {
                        operation: KernelFileOperation::Delete,
                        path: "/workspace/old.rs".to_owned(),
                        move_path: None,
                    },
                    KernelFileChange {
                        operation: KernelFileOperation::Create,
                        path: "/workspace/new.rs".to_owned(),
                        move_path: None,
                    },
                    KernelFileChange {
                        operation: KernelFileOperation::Write,
                        path: "/workspace/source.rs".to_owned(),
                        move_path: Some("/workspace/moved.rs".to_owned()),
                    },
                ],
            },
        })
        .expect("mixed patch requests");
        assert_eq!(files.len(), 3);
        assert!(matches!(
            &files[0],
            ToolRequest::File(file)
                if file.operation == winwincode_execution_port::action_normalizer::FileOperation::Create
                    && file.paths == ["/workspace/new.rs"]
        ));
        assert!(matches!(
            &files[1],
            ToolRequest::File(file)
                if file.operation == winwincode_execution_port::action_normalizer::FileOperation::Write
                    && file.paths == ["/workspace/moved.rs", "/workspace/source.rs"]
        ));
        assert!(matches!(
            &files[2],
            ToolRequest::File(file)
                if file.operation == winwincode_execution_port::action_normalizer::FileOperation::Delete
                    && file.paths == ["/workspace/old.rs"]
        ));
    }
}
