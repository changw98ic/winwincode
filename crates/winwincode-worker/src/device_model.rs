// SPDX-License-Identifier: Apache-2.0

//! Device-local model lane. Only execution control and public task events use the server port.

use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    io::Write,
    path::{Path, PathBuf},
    sync::mpsc,
};
use winwincode_execution_port::generated::{ExecutionPortMessage, ModelChunkMessage};
use winwincode_provider::{DeviceProviderError, DeviceProviderStore, model_failure};

pub(crate) struct DeviceModels {
    directory: PathBuf,
    pending: HashMap<String, String>,
    sender: mpsc::Sender<(String, Vec<ModelChunkMessage>)>,
    receiver: mpsc::Receiver<(String, Vec<ModelChunkMessage>)>,
    chunks: VecDeque<ModelChunkMessage>,
    /// Highest Provider sequence already handed to the Worker poll loop.
    /// Used to re-pull durable Device frames after an in-memory drop or a
    /// process restart so a post-tool second model call cannot stall at
    /// sequence 1 while Device already stores the full response.
    delivered_high_water: HashMap<String, i64>,
    /// Exchange identities this Worker opened through DeviceModels.
    /// Shared Device providers.sqlite3 stores every session's exchanges;
    /// recover must not re-queue foreign-session frames into this Worker.
    owned_exchanges: HashSet<String>,
    /// Session identities observed on ModelOpen messages this Worker sent.
    session_ids: HashSet<String>,
    /// Worker instance identities that may own Device frames for this process.
    instance_ids: HashSet<String>,
    /// Foreign exchanges already logged once (avoid 25ms poll spam).
    logged_foreign: HashSet<String>,
    /// Optional durable non-secret intake log shared with model_bridge.
    intake_log: Option<PathBuf>,
}

impl DeviceModels {
    pub(crate) fn open(directory: &Path) -> Result<Self, DeviceProviderError> {
        DeviceProviderStore::open(directory)?;
        let intake_log = std::env::var_os("WWC_MODEL_INTAKE_LOG")
            .map(PathBuf::from)
            .or_else(|| {
                // Managed workers keep durable diagnostics beside Codex runtime.
                // The provider directory itself is shared across Worker sessions
                // on one Device, so prefer an explicit env path when present.
                None
            });
        let (sender, receiver) = mpsc::channel();
        Ok(Self {
            directory: directory.to_owned(),
            pending: HashMap::new(),
            sender,
            receiver,
            chunks: VecDeque::new(),
            delivered_high_water: HashMap::new(),
            owned_exchanges: HashSet::new(),
            session_ids: HashSet::new(),
            instance_ids: HashSet::new(),
            logged_foreign: HashSet::new(),
            intake_log,
        })
    }

    pub(crate) fn set_intake_log(&mut self, path: Option<PathBuf>) {
        if path.is_some() {
            self.intake_log = path;
        }
    }

    pub(crate) fn note_worker_instance_id(&mut self, worker_instance_id: &str) {
        if !worker_instance_id.is_empty() {
            self.instance_ids.insert(worker_instance_id.to_owned());
        }
    }

    pub(crate) fn owns_exchange(&self, exchange_id: &str) -> bool {
        self.owned_exchanges.contains(exchange_id)
    }

    fn note_open_identity(
        &mut self,
        open: &winwincode_execution_port::generated::ModelOpenMessage,
    ) {
        self.owned_exchanges
            .insert(open.model_exchange_id.0.clone());
        let session = &open.session_identity;
        if !session.worker_session_id.0.is_empty() {
            self.session_ids.insert(session.worker_session_id.0.clone());
        }
        if !open.worker_session_id.0.is_empty() {
            self.session_ids.insert(open.worker_session_id.0.clone());
        }
        if !open.lease.worker_instance_id.0.is_empty() {
            self.instance_ids
                .insert(open.lease.worker_instance_id.0.clone());
        }
    }

    fn exchange_is_recoverable(
        &self,
        store: &DeviceProviderStore,
        exchange_id: &str,
    ) -> Result<bool, DeviceProviderError> {
        if self.owned_exchanges.contains(exchange_id) {
            return Ok(true);
        }
        // After a process restart the live owned set is empty until ModelOpen
        // is replayed. Durable Device frames still carry the session/instance
        // identity of the Worker that opened the exchange; recover those.
        let frames = store.replay_model(exchange_id, 1)?;
        let Some(first) = frames.first() else {
            return Ok(false);
        };
        let session = first.session_identity.worker_session_id.0.as_str();
        let instance = first.lease.worker_instance_id.0.as_str();
        Ok(self.session_ids.contains(session) || self.instance_ids.contains(instance))
    }

    pub(crate) fn send(
        &mut self,
        message: &ExecutionPortMessage,
    ) -> Result<bool, DeviceProviderError> {
        match message {
            ExecutionPortMessage::ModelOpenMessage(open) => {
                let id = open.model_exchange_id.0.clone();
                let digest = format!("{:x}", Sha256::digest(serde_json::to_vec(open)?));
                self.note_open_identity(open);
                if let Some(previous) = self.pending.get(&id) {
                    return if previous == &digest {
                        Ok(true)
                    } else {
                        Err(DeviceProviderError)
                    };
                }
                if self.pending.len() >= 16 {
                    return Err(DeviceProviderError);
                }
                self.pending.insert(id.clone(), digest);
                let directory = self.directory.clone();
                let sender = self.sender.clone();
                let open = open.clone();
                // ponytail: bounded blocking HTTPS runs outside the Worker loop; adapter deadlines
                // bound cancellation cleanup while heartbeats and tool approval remain responsive.
                if std::thread::Builder::new()
                    .name("device-provider".to_owned())
                    .spawn(move || {
                        let chunks = DeviceProviderStore::open(&directory)
                            .and_then(|store| store.execute_model(&open))
                            .unwrap_or_else(|_| {
                                vec![model_failure(
                                    &open,
                                    "DEVICE_MODEL_FAILED: local request could not complete",
                                )]
                            });
                        let _ = sender.send((open.model_exchange_id.0, chunks));
                    })
                    .is_err()
                {
                    self.pending.remove(&id);
                    return Err(DeviceProviderError);
                }
                Ok(true)
            }
            ExecutionPortMessage::ModelAckMessage(ack) => {
                let store = DeviceProviderStore::open(&self.directory)?;
                if ack.error.is_some() {
                    store.cancel_model(&ack.model_exchange_id.0)?;
                    self.chunks
                        .retain(|chunk| chunk.model_exchange_id != ack.model_exchange_id);
                } else if let Some(from) = &ack.replay_from_sequence {
                    self.chunks
                        .extend(store.replay_model(&ack.model_exchange_id.0, from.0)?);
                }
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    pub(crate) fn retry_chunk(&mut self, chunk: ModelChunkMessage) {
        self.chunks.push_front(chunk);
    }

    /// Drops a non-owned Device frame without pinning this Worker's queue.
    pub(crate) fn drop_chunk(&mut self, chunk: &ModelChunkMessage) {
        self.chunks
            .retain(|queued| queued.model_exchange_id != chunk.model_exchange_id
                || queued.sequence != chunk.sequence);
    }

    pub(crate) fn next_chunk(&mut self) -> Result<Option<ModelChunkMessage>, DeviceProviderError> {
        while let Ok((id, chunks)) = self.receiver.try_recv() {
            self.pending.remove(&id);
            if !DeviceProviderStore::open(&self.directory)?.model_cancelled(&id)? {
                self.owned_exchanges.insert(id.clone());
                if let Some(first) = chunks.first() {
                    let session = first.session_identity.worker_session_id.0.clone();
                    if !session.is_empty() {
                        self.session_ids.insert(session);
                    }
                    let instance = first.lease.worker_instance_id.0.clone();
                    if !instance.is_empty() {
                        self.instance_ids.insert(instance);
                    }
                }
                self.chunks.extend(chunks);
            }
        }
        if self.chunks.is_empty() {
            self.recover_from_durable_store()?;
        }
        let Some(chunk) = self.chunks.pop_front() else {
            return Ok(None);
        };
        let high_water = self
            .delivered_high_water
            .entry(chunk.model_exchange_id.0.clone())
            .or_insert(0);
        if chunk.sequence.0 > *high_water {
            *high_water = chunk.sequence.0;
        }
        Ok(Some(chunk))
    }

    /// Re-queues durable Provider frames above the last delivered sequence.
    ///
    /// Device providers.sqlite3 is authoritative for completed exchanges. After
    /// an in-memory drop or a Worker restart the live queue can stall at
    /// sequence 1 even though later frames already exist; replaying only the
    /// missing tail keeps the durable model-call ledger contiguous.
    ///
    /// The Device store is shared by every Worker session on one Device.
    /// Recovering another session's exchange and then failing authority would
    /// pin this Worker's in-memory queue on a foreign frame and starve its own
    /// verification exchange. Only owned / identity-matching exchanges are
    /// re-queued.
    fn recover_from_durable_store(&mut self) -> Result<(), DeviceProviderError> {
        let store = DeviceProviderStore::open(&self.directory)?;
        for exchange_id in store.list_stored_model_exchanges()? {
            if self.pending.contains_key(&exchange_id) {
                continue;
            }
            if !self.exchange_is_recoverable(&store, &exchange_id)? {
                if self.logged_foreign.insert(exchange_id.clone()) {
                    self.log_intake(
                        "recover_skip",
                        "FOREIGN_EXCHANGE",
                        &exchange_id,
                        0,
                        "",
                        "",
                        "shared Device store exchange not owned by this Worker",
                    );
                }
                continue;
            }
            let high_water = self
                .delivered_high_water
                .get(&exchange_id)
                .copied()
                .unwrap_or(0);
            let frames = store.replay_model(&exchange_id, high_water.saturating_add(1))?;
            if frames.is_empty() {
                continue;
            }
            for frame in &frames {
                let entry = self
                    .delivered_high_water
                    .entry(exchange_id.clone())
                    .or_insert(0);
                if frame.sequence.0 > *entry {
                    *entry = frame.sequence.0;
                }
            }
            self.owned_exchanges.insert(exchange_id.clone());
            self.chunks.extend(frames);
        }
        Ok(())
    }

    pub(crate) fn log_intake(
        &self,
        stage: &str,
        code: &str,
        exchange_id: &str,
        sequence: i64,
        worker_session_id: &str,
        thread_id: &str,
        detail: &str,
    ) {
        let Some(path) = self.intake_log.as_deref() else {
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
            "ts={ts} component=device_models stage={stage} code={code} exchange={exchange_id} seq={sequence} wsn={worker_session_id} thr={thread_id} detail={detail}"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use winwincode_domain::{
        CodexThreadId, ExecutionJobId, ExecutionMessageId, ExecutionSequence, FencingToken,
        Instant, LeaseId, ModelExchangeId, ProductSessionId, SchemaVersion, SessionIdentity,
        Sha256Digest, WorkerId, WorkerInstanceId, WorkerSessionId,
    };
    use winwincode_execution_port::generated::{
        EncodedPayload, ExecutionLeaseStamp, ModelChunkMessageKind,
    };

    fn authority_session(thread: &str, worker_session_id: &str) -> SessionIdentity {
        let worker_session_id = WorkerSessionId(worker_session_id.to_owned());
        SessionIdentity {
            codex_thread_id: CodexThreadId(thread.to_owned()),
            product_session_id: ProductSessionId(format!("psn_{}", "C".repeat(26))),
            work_run_id: None,
            worker_session_id,
        }
    }

    fn chunk(
        exchange: &str,
        sequence: i64,
        is_final: bool,
        worker_session_id: &str,
        worker_instance_id: &str,
    ) -> ModelChunkMessage {
        let session = authority_session("cdx_post_tool_second_call", worker_session_id);
        let payload_bytes: &[u8] = if sequence == 1 {
            br#"{"type":"created"}"#
        } else if is_final {
            br#"{"type":"completed","endTurn":true}"#
        } else {
            br#"{"type":"server_model","model":"loopback"}"#
        };
        ModelChunkMessage {
            error: None,
            is_final,
            kind: ModelChunkMessageKind::ModelChunk,
            lease: ExecutionLeaseStamp {
                attempt: 1,
                expires_at: Instant("2030-01-01T01:00:00.000Z".to_owned()),
                fencing_token: FencingToken("1".to_owned()),
                issued_at: Instant("2030-01-01T00:00:00.000Z".to_owned()),
                job_id: ExecutionJobId(format!("job_{}", "D".repeat(26))),
                lease_id: LeaseId(format!("lse_{}", "E".repeat(26))),
                worker_id: WorkerId(format!("wrk_{}", "F".repeat(26))),
                worker_instance_id: WorkerInstanceId(worker_instance_id.to_owned()),
            },
            message_id: ExecutionMessageId(format!("xmsg_0{exchange}{sequence}")),
            model_exchange_id: ModelExchangeId(exchange.to_owned()),
            payload: Some(EncodedPayload {
                content_type: "application/json".to_owned(),
                data_base64: base64::Engine::encode(
                    &base64::engine::general_purpose::STANDARD,
                    payload_bytes,
                ),
                payload_digest: Sha256Digest(format!(
                    "sha256:{:x}",
                    Sha256::digest(payload_bytes)
                )),
            }),
            schema_version: SchemaVersion::WinwincodeV1,
            sent_at: Instant("2030-01-01T00:00:00.000Z".to_owned()),
            sequence: ExecutionSequence(sequence),
            session_identity: session.clone(),
            worker_session_id: session.worker_session_id,
        }
    }

    fn temp_root(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "winwincode-device-models-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ))
    }

    #[test]
    fn durable_provider_frames_are_replayed_above_the_delivered_high_water() {
        let root = temp_root("replay");
        let exchange = format!("mdl_{}", "H".repeat(26));
        let wsn = format!("wsn_{}", "B".repeat(26));
        let wki = format!("wki_{}", "G".repeat(26));
        {
            let store = DeviceProviderStore::open(&root).expect("open device provider store");
            let chunks = vec![
                chunk(&exchange, 1, false, &wsn, &wki),
                chunk(&exchange, 2, false, &wsn, &wki),
                chunk(&exchange, 3, true, &wsn, &wki),
            ];
            store
                .retain_stored_model_exchange_chunks(&exchange, &chunks)
                .expect("seed durable provider exchange");
        }
        let mut models = DeviceModels::open(&root).expect("open device models");
        models.owned_exchanges.insert(exchange.clone());
        models.delivered_high_water.insert(exchange.clone(), 1);
        let recovered: Vec<_> = std::iter::from_fn(|| models.next_chunk().unwrap())
            .take(2)
            .collect();
        assert_eq!(
            recovered
                .iter()
                .map(|chunk| chunk.sequence.0)
                .collect::<Vec<_>>(),
            vec![2, 3]
        );
        assert!(recovered.last().expect("final recovered frame").is_final);
        assert!(models.next_chunk().unwrap().is_none());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn verification_exchange_recover_skips_foreign_device_store_frames() {
        let root = temp_root("foreign");
        let foreign_exchange = format!("mdl_{}", "A".repeat(26));
        let owned_exchange = format!("mdl_{}", "Z".repeat(26));
        let foreign_wsn = format!("wsn_{}", "F".repeat(26));
        let owned_wsn = format!("wsn_{}", "O".repeat(26));
        let foreign_wki = format!("wki_{}", "1".repeat(26));
        let owned_wki = format!("wki_{}", "2".repeat(26));
        {
            let store = DeviceProviderStore::open(&root).expect("open device provider store");
            store
                .retain_stored_model_exchange_chunks(
                    &foreign_exchange,
                    &[
                        chunk(&foreign_exchange, 1, false, &foreign_wsn, &foreign_wki),
                        chunk(&foreign_exchange, 2, true, &foreign_wsn, &foreign_wki),
                    ],
                )
                .expect("seed foreign exchange");
            store
                .retain_stored_model_exchange_chunks(
                    &owned_exchange,
                    &[
                        chunk(&owned_exchange, 1, false, &owned_wsn, &owned_wki),
                        chunk(&owned_exchange, 2, false, &owned_wsn, &owned_wki),
                        chunk(
                            &owned_exchange,
                            3,
                            true,
                            &owned_wsn,
                            &owned_wki,
                        ),
                    ],
                )
                .expect("seed owned verification exchange");
        }
        let mut models = DeviceModels::open(&root).expect("open device models");
        models.note_worker_instance_id(&owned_wki);
        models
            .session_ids
            .insert(owned_wsn.clone());
        // Simulate the verification ModelOpen that marked ownership.
        models.owned_exchanges.insert(owned_exchange.clone());

        let recovered: Vec<_> = std::iter::from_fn(|| models.next_chunk().unwrap()).collect();
        assert!(
            recovered
                .iter()
                .all(|chunk| chunk.model_exchange_id.0 == owned_exchange),
            "recover must not re-queue foreign Device-store exchanges, got sequences {:?}",
            recovered
                .iter()
                .map(|chunk| (
                    chunk.model_exchange_id.0.clone(),
                    chunk.sequence.0
                ))
                .collect::<Vec<_>>()
        );
        assert_eq!(
            recovered
                .iter()
                .map(|chunk| chunk.sequence.0)
                .collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
        assert!(models.next_chunk().unwrap().is_none());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn verification_exchange_recover_matches_session_identity_after_restart() {
        let root = temp_root("restart");
        let exchange = format!("mdl_{}", "V".repeat(26));
        let wsn = format!("wsn_{}", "R".repeat(26));
        let wki = format!("wki_{}", "K".repeat(26));
        {
            let store = DeviceProviderStore::open(&root).expect("open device provider store");
            store
                .retain_stored_model_exchange_chunks(
                    &exchange,
                    &[
                        chunk(&exchange, 1, false, &wsn, &wki),
                        chunk(&exchange, 2, true, &wsn, &wki),
                    ],
                )
                .expect("seed durable verification exchange");
        }
        // Fresh process: owned_exchanges empty, but Worker re-seeded identity.
        let mut models = DeviceModels::open(&root).expect("open device models");
        models.session_ids.insert(wsn.clone());
        models.note_worker_instance_id(&wki);
        let recovered: Vec<_> = std::iter::from_fn(|| models.next_chunk().unwrap()).collect();
        assert_eq!(
            recovered
                .iter()
                .map(|chunk| chunk.sequence.0)
                .collect::<Vec<_>>(),
            vec![1, 2]
        );
        let _ = std::fs::remove_dir_all(root);
    }
}
