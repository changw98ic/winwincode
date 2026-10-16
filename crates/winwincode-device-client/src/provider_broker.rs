// SPDX-License-Identifier: Apache-2.0

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

use winwincode_client_port::private_worker_channel::{
    PrivateWorkerResponseBody, ProviderExecutionProfile, ProviderOperation, SensitivePayload,
};
use winwincode_client_port::worker_relay::{WorkerRelayRequest, WorkerRelayResponse};
use winwincode_execution_port::execution_identity::{
    retained_lease_matches_current, valid_lease_renewal,
};
use winwincode_execution_port::generated::{
    ExecutionLeaseStamp, ExecutionPortMessage, ExecutionScope, JobDispatchMessage,
};
use winwincode_execution_port::transport::{
    RemoteExchangeRequest, RemoteExchangeResponse, RemoteTransportAdapter,
};
use winwincode_provider::{
    DeviceProviderError, DeviceProviderStore, LocalDeviceProviderSource, ProviderExecutionPurpose,
    ProviderExecutionRequest, ProviderExecutionSource,
};

struct NoopCore;
impl winwincode_execution_port::transport::ExecutionPortCore for NoopCore {
    type Output = ();
    type Error = std::convert::Infallible;
    fn accept(&mut self, _: &ExecutionPortMessage) -> Result<(), Self::Error> {
        Ok(())
    }
}

struct Attempt {
    dispatch: JobDispatchMessage,
    deadline: Instant,
    cancelled: bool,
}

pub(crate) struct ProviderBroker {
    directory: PathBuf,
    home: PathBuf,
    worker_id: String,
    worker_instance_id: String,
    product_session_id: Option<String>,
    work_run_id: Option<String>,
    profile: ProviderExecutionProfile,
    live_process: Arc<dyn Fn() -> bool + Send + Sync>,
    attempts: Mutex<BTreeMap<String, Attempt>>,
    closed: AtomicBool,
    jev_settings_file: Option<PathBuf>,
    jev_imported: Mutex<BTreeSet<String>>,
}

impl ProviderBroker {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        directory: PathBuf,
        home: PathBuf,
        worker_id: String,
        worker_instance_id: String,
        product_session_id: Option<String>,
        work_run_id: Option<String>,
        profile: ProviderExecutionProfile,
        live_process: Arc<dyn Fn() -> bool + Send + Sync>,
    ) -> Self {
        Self {
            directory,
            home,
            worker_id,
            worker_instance_id,
            product_session_id,
            work_run_id,
            profile,
            live_process,
            attempts: Mutex::new(BTreeMap::new()),
            closed: AtomicBool::new(false),
            jev_settings_file: std::env::var_os("WWC_DEVICE_JEV_SETTINGS_FILE").map(PathBuf::from),
            jev_imported: Mutex::new(BTreeSet::new()),
        }
    }

    pub(crate) fn close(&self) {
        self.closed.store(true, Ordering::Release);
    }

    fn active(&self) -> bool {
        !self.closed.load(Ordering::Acquire) && (self.live_process)()
    }

    fn lease_matches(&self, lease: &ExecutionLeaseStamp, current: &Attempt) -> bool {
        self.active()
            && !current.cancelled
            && Instant::now() < current.deadline
            && lease.worker_id.0 == self.worker_id
            && lease.worker_instance_id.0 == self.worker_instance_id
            && retained_lease_matches_current(lease, &current.dispatch.lease)
    }

    fn scope_matches(&self, dispatch: &JobDispatchMessage) -> bool {
        let (session, run) = match &dispatch.job.scope {
            ExecutionScope::ProductSessionExecutionScope(scope) => {
                (&scope.product_session_id.0, None)
            }
            ExecutionScope::WorkRunExecutionScope(scope) => (
                &scope.product_session_id.0,
                Some(scope.work_run_id.0.as_str()),
            ),
        };
        self.product_session_id
            .as_ref()
            .is_none_or(|expected| expected == session)
            && self
                .work_run_id
                .as_deref()
                .is_none_or(|expected| Some(expected) == run)
    }

    fn allowed(&self, request: &ProviderExecutionRequest) -> bool {
        let Some(context) = &request.context else {
            return false;
        };
        let Ok(attempts) = self.attempts.lock() else {
            return false;
        };
        let Some(current) = attempts.get(&context.lease.job_id.0) else {
            return false;
        };
        let (session, run) = match &current.dispatch.job.scope {
            ExecutionScope::ProductSessionExecutionScope(scope) => {
                (&scope.product_session_id.0, None)
            }
            ExecutionScope::WorkRunExecutionScope(scope) => (
                &scope.product_session_id.0,
                Some(scope.work_run_id.0.as_str()),
            ),
        };
        if !self.lease_matches(&context.lease, current)
            || context.session_identity.worker_session_id != context.worker_session_id
            || context.worker_session_id.0.is_empty()
            || context.session_identity.codex_thread_id.0.is_empty()
            || context.session_identity.product_session_id.0 != *session
            || context
                .session_identity
                .work_run_id
                .as_ref()
                .map(|id| id.0.as_str())
                != run
            || context.model_exchange_id.0.is_empty()
            || context.request_id.0.is_empty()
        {
            return false;
        }
        match &request.purpose {
            ProviderExecutionPurpose::Model => {
                let selection = current.dispatch.job.model_selection.as_ref();
                let provider = selection
                    .map(|value| value.provider_id.as_str())
                    .or(self.profile.provider.as_deref());
                let model = selection
                    .map(|value| value.model_id.as_str())
                    .or(self.profile.model.as_deref());
                provider == Some(request.provider_id.as_str())
                    && model == request.model_id.as_deref()
            }
            ProviderExecutionPurpose::Observer => {
                self.profile.observer_mode
                    == winwincode_execution_port::runtime_trace_outbox::ObserverMode::AmbiguousOnly
                    && self.profile.observer_provider.as_deref()
                        == Some(request.provider_id.as_str())
                    && self.profile.observer_model.as_deref() == request.model_id.as_deref()
            }
            ProviderExecutionPurpose::Fusion { member_id } => {
                self.profile.fusion.as_ref().is_some_and(|fusion| {
                    fusion.members.iter().any(|member| {
                        member.id == *member_id
                            && member.provider == request.provider_id
                            && Some(member.model.as_str()) == request.model_id.as_deref()
                    })
                })
            }
            ProviderExecutionPurpose::JevContext { operation_id } => {
                !operation_id.is_empty()
                    && self
                        .profile
                        .jev_context
                        .as_ref()
                        .is_some_and(|settings| settings.provider == request.provider_id)
            }
            ProviderExecutionPurpose::JevJudge { operation_id } => {
                !operation_id.is_empty()
                    && self.profile.jev_judge.as_deref() == Some(request.provider_id.as_str())
            }
        }
    }

    fn import_jev(&self, provider: &str) -> Result<(), DeviceProviderError> {
        let Some(file) = &self.jev_settings_file else {
            return Ok(());
        };
        let mut imported = self
            .jev_imported
            .lock()
            .map_err(|_| DeviceProviderError::StateInvalid)?;
        if imported.contains(provider) {
            return Ok(());
        }
        DeviceProviderStore::open(&self.directory)?.import_jev_settings(file, provider)?;
        imported.insert(provider.to_owned());
        Ok(())
    }

    pub(crate) fn provider(
        &self,
        operation: ProviderOperation,
        body: &str,
    ) -> PrivateWorkerResponseBody {
        let result = (|| {
            let request: ProviderExecutionRequest = serde_json::from_str(body)?;
            if !self.allowed(&request) {
                return Err(DeviceProviderError::AuthorityEnded);
            }
            if matches!(
                request.purpose,
                ProviderExecutionPurpose::JevContext { .. }
                    | ProviderExecutionPurpose::JevJudge { .. }
            ) {
                self.import_jev(&request.provider_id)?;
            }
            let source = LocalDeviceProviderSource::new(self.directory.clone());
            let bytes = match operation {
                ProviderOperation::ModelParameters => {
                    if !matches!(
                        request.purpose,
                        ProviderExecutionPurpose::Model
                            | ProviderExecutionPurpose::Observer
                            | ProviderExecutionPurpose::Fusion { .. }
                    ) {
                        return Err(DeviceProviderError::StateInvalid);
                    }
                    serde_json::to_vec(&source.model_parameters(&request)?)?
                }
                ProviderOperation::JevParameters => {
                    if !matches!(
                        request.purpose,
                        ProviderExecutionPurpose::JevContext { .. }
                            | ProviderExecutionPurpose::JevJudge { .. }
                    ) {
                        return Err(DeviceProviderError::StateInvalid);
                    }
                    serde_json::to_vec(&source.jev_parameters(&request)?)?
                }
                ProviderOperation::ResolveConnection => {
                    if request.physical_attempt.is_none_or(|attempt| attempt < 1)
                        || (matches!(
                            request.purpose,
                            ProviderExecutionPurpose::Model
                                | ProviderExecutionPurpose::Fusion { .. }
                        ) && request
                            .adapter_request_id
                            .as_ref()
                            .is_none_or(String::is_empty))
                    {
                        return Err(DeviceProviderError::StateInvalid);
                    }
                    serde_json::to_vec(
                        &source.resolve_connection(&request, &|| self.allowed(&request))?,
                    )?
                }
            };
            let payload = SensitivePayload(bytes);
            if !self.allowed(&request) {
                return Err(DeviceProviderError::AuthorityEnded);
            }
            Ok(payload)
        })();
        local_response(result)
    }

    pub(crate) fn extensions(&self, body: &str) -> PrivateWorkerResponseBody {
        let result = (|| {
            let lease: ExecutionLeaseStamp = serde_json::from_str(body)?;
            let benchmark = {
                let attempts = self
                    .attempts
                    .lock()
                    .map_err(|_| DeviceProviderError::StateInvalid)?;
                let current = attempts
                    .get(&lease.job_id.0)
                    .ok_or(DeviceProviderError::AuthorityEnded)?;
                if !self.lease_matches(&lease, current) {
                    return Err(DeviceProviderError::AuthorityEnded);
                }
                winwincode_provider::benchmark_mcp_server(
                    &current.dispatch.job,
                    self.profile.sealed_benchmark_tools,
                )?
                .map(str::to_owned)
            };
            let store = DeviceProviderStore::open(&self.directory)?;
            let tools = if let Some(server) = benchmark {
                store.refresh_benchmark_extensions(&self.home, &server)?
            } else {
                store.refresh_extensions(&self.home)?
            };
            if !self.active() {
                return Err(DeviceProviderError::AuthorityEnded);
            }
            Ok(SensitivePayload(serde_json::to_vec(&tools)?))
        })();
        local_response(result)
    }

    pub(crate) fn observe(
        &self,
        request: &WorkerRelayRequest,
        reply: &WorkerRelayResponse,
        started: Instant,
    ) {
        if reply.status == 401 || reply.status == 403 {
            self.close();
        }
        let WorkerRelayRequest::Exchange { body } = request else {
            return;
        };
        if reply.status != 200 || reply.failure.is_some() {
            return;
        }
        let Ok(response) = RemoteExchangeResponse::decode(reply.body.as_bytes()) else {
            return;
        };
        if !response.has_acceptance_receipt() {
            return;
        }
        let Ok(mut attempts) = self.attempts.lock() else {
            self.close();
            return;
        };
        if response.frame_accepted()
            && let Ok(sent) = RemoteExchangeRequest::decode(body.as_bytes())
            && let Ok(frame) = RemoteTransportAdapter::<NoopCore>::decode(sent.frame())
            && let ExecutionPortMessage::JobOutcomeMessage(outcome) = frame.message()
            && let Some(current) = attempts.get_mut(&outcome.lease.job_id.0)
            && retained_lease_matches_current(&outcome.lease, &current.dispatch.lease)
        {
            current.cancelled = true;
        }
        let server_time = time::OffsetDateTime::parse(
            &response.server_time().0,
            &time::format_description::well_known::Rfc3339,
        );
        for delivery in response.deliveries() {
            let Ok(frame) = RemoteTransportAdapter::<NoopCore>::decode(&delivery.frame) else {
                self.close();
                break;
            };
            match frame.message() {
                ExecutionPortMessage::JobDispatchMessage(dispatch) => self.observe_dispatch(
                    &mut attempts,
                    dispatch,
                    server_time.as_ref().ok(),
                    started,
                ),
                ExecutionPortMessage::SnapshotVerificationDispatchMessage(message) => self
                    .observe_dispatch(
                        &mut attempts,
                        &message.dispatch,
                        server_time.as_ref().ok(),
                        started,
                    ),
                ExecutionPortMessage::LeaseRenewMessage(renewal) => {
                    if let Some(current) = attempts.get_mut(&renewal.lease.job_id.0)
                        && current.dispatch.lease != renewal.lease
                        && valid_lease_renewal(
                            &current.dispatch.lease,
                            renewal,
                            response.server_time(),
                        )
                        && let Some(deadline) =
                            lease_deadline(&renewal.lease, server_time.as_ref().ok(), started)
                    {
                        current.dispatch.lease = renewal.lease.clone();
                        current.deadline = deadline;
                    }
                }
                ExecutionPortMessage::JobCancelMessage(cancel) => {
                    if let Some(current) = attempts.get_mut(&cancel.lease.job_id.0)
                        && retained_lease_matches_current(&cancel.lease, &current.dispatch.lease)
                    {
                        current.cancelled = true;
                    }
                }
                _ => {}
            }
        }
    }

    fn observe_dispatch(
        &self,
        attempts: &mut BTreeMap<String, Attempt>,
        dispatch: &JobDispatchMessage,
        server_time: Option<&time::OffsetDateTime>,
        started: Instant,
    ) {
        if dispatch.lease.worker_id.0 != self.worker_id
            || dispatch.lease.worker_instance_id.0 != self.worker_instance_id
            || dispatch.job.job_id != dispatch.lease.job_id
            || dispatch.job.attempt != dispatch.lease.attempt
            || !self.scope_matches(dispatch)
        {
            return;
        }
        if let Some(old) = attempts.get(&dispatch.job.job_id.0)
            && (dispatch.lease.attempt <= old.dispatch.lease.attempt
                || dispatch.lease.fencing_token.0.parse::<u64>().ok()
                    <= old.dispatch.lease.fencing_token.0.parse::<u64>().ok())
        {
            return;
        }
        let Some(deadline) = lease_deadline(&dispatch.lease, server_time, started) else {
            return;
        };
        attempts.insert(
            dispatch.job.job_id.0.clone(),
            Attempt {
                dispatch: dispatch.clone(),
                deadline,
                cancelled: false,
            },
        );
    }
}

fn lease_deadline(
    lease: &ExecutionLeaseStamp,
    server_time: Option<&time::OffsetDateTime>,
    started: Instant,
) -> Option<Instant> {
    let expires = time::OffsetDateTime::parse(
        &lease.expires_at.0,
        &time::format_description::well_known::Rfc3339,
    )
    .ok()?;
    let lifetime = Duration::try_from(expires - *server_time?).ok()?;
    started
        .checked_add(lifetime)
        .filter(|deadline| *deadline > Instant::now())
}

pub(crate) fn local_response(
    result: Result<SensitivePayload, DeviceProviderError>,
) -> PrivateWorkerResponseBody {
    match result {
        Ok(payload) => PrivateWorkerResponseBody::Local {
            payload,
            error: None,
        },
        Err(error) => PrivateWorkerResponseBody::Local {
            payload: SensitivePayload(Vec::new()),
            error: Some(error.code().to_owned()),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use winwincode_provider::ProviderExecutionContext;

    fn fixture() -> (ProviderBroker, ProviderExecutionRequest, JobDispatchMessage) {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../../tests/fixtures/contracts/execution-port.valid.json"
        ))
        .unwrap();
        let message = |kind: &str| -> ExecutionPortMessage {
            serde_json::from_value(
                fixture["messages"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|value| value["kind"] == kind)
                    .unwrap()
                    .clone(),
            )
            .unwrap()
        };
        let ExecutionPortMessage::JobDispatchMessage(mut dispatch) = message("job.dispatch") else {
            panic!("dispatch fixture");
        };
        let ExecutionPortMessage::ModelOpenMessage(open) = message("model.open") else {
            panic!("model fixture");
        };
        dispatch.job.model_selection = Some(
            serde_json::from_value(
                serde_json::json!({"providerId":"provider-a","modelId":"model-a"}),
            )
            .unwrap(),
        );
        let mut context = ProviderExecutionContext::from(&open);
        context.lease = dispatch.lease.clone();
        let (session, run) = match &dispatch.job.scope {
            ExecutionScope::ProductSessionExecutionScope(scope) => {
                (scope.product_session_id.clone(), None)
            }
            ExecutionScope::WorkRunExecutionScope(scope) => (
                scope.product_session_id.clone(),
                Some(scope.work_run_id.clone()),
            ),
        };
        context.session_identity.product_session_id = session.clone();
        context.session_identity.work_run_id = run.clone();
        context.session_identity.worker_session_id = context.worker_session_id.clone();
        let broker = ProviderBroker::new(
            PathBuf::new(),
            PathBuf::new(),
            dispatch.lease.worker_id.0.clone(),
            dispatch.lease.worker_instance_id.0.clone(),
            Some(session.0),
            run.map(|id| id.0),
            ProviderExecutionProfile::default(),
            Arc::new(|| true),
        );
        broker.attempts.lock().unwrap().insert(
            dispatch.job.job_id.0.clone(),
            Attempt {
                dispatch: dispatch.clone(),
                deadline: Instant::now() + Duration::from_mins(1),
                cancelled: false,
            },
        );
        let request = ProviderExecutionRequest {
            context: Some(context),
            provider_id: "provider-a".into(),
            model_id: Some("model-a".into()),
            purpose: ProviderExecutionPurpose::Model,
            physical_attempt: Some(1),
            adapter_request_id: Some("physical-request".into()),
        };
        (broker, request, dispatch)
    }

    #[test]
    fn parent_authority_rejects_old_attempt_instance_scope_and_route() {
        let (broker, request, _) = fixture();
        assert!(broker.allowed(&request));
        let mut changed = request.clone();
        changed.context.as_mut().unwrap().lease.attempt += 1;
        assert!(!broker.allowed(&changed));
        let mut changed = request.clone();
        changed
            .context
            .as_mut()
            .unwrap()
            .lease
            .worker_instance_id
            .0
            .push('x');
        assert!(!broker.allowed(&changed));
        let mut changed = request.clone();
        changed
            .context
            .as_mut()
            .unwrap()
            .session_identity
            .product_session_id
            .0
            .push('x');
        assert!(!broker.allowed(&changed));
        let mut changed = request.clone();
        changed.provider_id = "another-provider".into();
        assert!(!broker.allowed(&changed));
        let mut changed = request.clone();
        changed.context = None;
        assert!(!broker.allowed(&changed));
    }

    #[test]
    fn recovered_session_is_correlation_under_the_same_parent_authority() {
        let (broker, mut request, _) = fixture();
        let context = request.context.as_mut().unwrap();
        context.worker_session_id.0 = "wsn_recovered_session".into();
        context.session_identity.worker_session_id = context.worker_session_id.clone();
        context.session_identity.codex_thread_id.0 = "cdx_recovered_thread".into();
        assert!(broker.allowed(&request));
        request
            .context
            .as_mut()
            .unwrap()
            .session_identity
            .worker_session_id
            .0
            .push('x');
        assert!(!broker.allowed(&request));
    }

    #[test]
    fn duplicate_dispatch_cannot_revive_cancelled_authority_or_extend_deadline() {
        let (broker, request, dispatch) = fixture();
        let mut attempts = broker.attempts.lock().unwrap();
        let current = attempts.get_mut(&dispatch.job.job_id.0).unwrap();
        current.cancelled = true;
        let deadline = current.deadline;
        let server_time = time::OffsetDateTime::parse(
            &dispatch.lease.issued_at.0,
            &time::format_description::well_known::Rfc3339,
        )
        .unwrap();
        broker.observe_dispatch(&mut attempts, &dispatch, Some(&server_time), Instant::now());
        assert!(attempts[&dispatch.job.job_id.0].cancelled);
        assert_eq!(attempts[&dispatch.job.job_id.0].deadline, deadline);
        drop(attempts);
        assert!(!broker.allowed(&request));
    }
    #[test]
    fn observer_uses_only_the_parent_configured_route() {
        let (mut broker, mut request, _) = fixture();
        request.purpose = ProviderExecutionPurpose::Observer;
        assert!(!broker.allowed(&request));
        broker.profile.observer_mode =
            winwincode_execution_port::runtime_trace_outbox::ObserverMode::AmbiguousOnly;
        broker.profile.observer_provider = Some("observer-provider".into());
        broker.profile.observer_model = Some("observer-model".into());
        assert!(!broker.allowed(&request));
        request.provider_id = "observer-provider".into();
        request.model_id = Some("observer-model".into());
        assert!(broker.allowed(&request));
        request.context.as_mut().unwrap().lease.attempt += 1;
        assert!(!broker.allowed(&request));
    }
    #[test]
    fn failed_jev_import_can_retry_in_the_same_broker_after_the_file_is_repaired() {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let (mut broker, _, _) = fixture();
        let root = std::env::temp_dir().join(format!(
            "wwc-broker-import-{}-{}",
            std::process::id(),
            time::OffsetDateTime::now_utc().unix_timestamp_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        broker.directory = root.join("providers");
        let file = root.join("private-jev.toml");
        broker.jev_settings_file = Some(file.clone());
        assert!(broker.import_jev("device-jev").is_err());
        assert!(!broker.jev_imported.lock().unwrap().contains("device-jev"));
        let mut settings = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&file)
            .unwrap();
        settings
            .write_all(
                br#"providerId = "device-jev"
endpoint = "https://nli.invalid/score"
apiKey = "fixture-key"
modelId = "jev-fixture"
timeoutMs = 1000
retries = 0
maxBatchSize = 4
devices = ["remote"]
dtypes = ["auto"]
"#,
            )
            .unwrap();
        drop(settings);
        broker.import_jev("device-jev").unwrap();
        assert!(broker.jev_imported.lock().unwrap().contains("device-jev"));
        std::fs::write(&file, "invalid updated file").unwrap();
        broker.import_jev("device-jev").unwrap();
        let saved = DeviceProviderStore::open(&broker.directory)
            .unwrap()
            .resolve_jev_settings("device-jev")
            .unwrap();
        assert_eq!(saved.model_id.as_deref(), Some("jev-fixture"));
        std::fs::remove_dir_all(root).unwrap();
    }
}
