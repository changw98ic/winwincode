// SPDX-License-Identifier: Apache-2.0

//! Device-local model lane. Only execution control and public task events use the server port.

use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, VecDeque},
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
}

impl DeviceModels {
    pub(crate) fn open(directory: &Path) -> Result<Self, DeviceProviderError> {
        DeviceProviderStore::open(directory)?;
        let (sender, receiver) = mpsc::channel();
        Ok(Self {
            directory: directory.to_owned(),
            pending: HashMap::new(),
            sender,
            receiver,
            chunks: VecDeque::new(),
        })
    }

    pub(crate) fn send(
        &mut self,
        message: &ExecutionPortMessage,
    ) -> Result<bool, DeviceProviderError> {
        match message {
            ExecutionPortMessage::ModelOpenMessage(open) => {
                let id = open.model_exchange_id.0.clone();
                let digest = format!("{:x}", Sha256::digest(serde_json::to_vec(open)?));
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

    pub(crate) fn next_chunk(&mut self) -> Result<Option<ModelChunkMessage>, DeviceProviderError> {
        while let Ok((id, chunks)) = self.receiver.try_recv() {
            self.pending.remove(&id);
            if !DeviceProviderStore::open(&self.directory)?.model_cancelled(&id)? {
                self.chunks.extend(chunks);
            }
        }
        Ok(self.chunks.pop_front())
    }
}
