// SPDX-License-Identifier: Apache-2.0

//! CRI RuntimeService implementation over the engine + store.
//!
//! Handlers are thin: validate, translate through [`crate::convert`], call
//! the (blocking) engine inside `spawn_blocking`, persist records, and emit
//! container events. Long-running containers are watched by a monitor thread
//! per container that owns `wait_container` and finalises the record.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use tokio::sync::broadcast;
use tonic::{Request, Response, Status};

use crate::api::runtime_service_server::RuntimeService;
use crate::api::*;
use crate::config::{API_VERSION, Config};
use crate::convert::{self, NamespacePaths};
use crate::engine::{Engine, EngineImage};
use crate::error::{Error, ErrorCode, Result as CrateResult};
use crate::logs::CriLogWriter;
use crate::network;
use crate::store::{
    ContainerRecord, ContainerState as StoredState, Metadata as StoredMetadata, SandboxRecord,
    SandboxState, Store, new_id, now_ns,
};
use crate::streaming::{SessionKind, StreamingState};
use prost::Message;

type BoxStream<T> =
    Pin<Box<dyn tonic::codegen::tokio_stream::Stream<Item = Result<T, Status>> + Send>>;

pub(crate) struct Inner {
    pub config: Config,
    pub engine: Arc<dyn Engine>,
    pub store: Arc<Store>,
    pub events: broadcast::Sender<ContainerEventResponse>,
    pub logs: RwLock<HashMap<String, Arc<CriLogWriter>>>,
    pub streaming: Arc<StreamingState>,
    pub pod_cidr: RwLock<String>,
}

/// The CRI service. One value implements both RuntimeService and
/// ImageService (the latter in `image_service.rs`).
pub struct CriService {
    pub(crate) inner: Arc<Inner>,
}

impl Clone for CriService {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl CriService {
    /// Clone this service handle (shares all state).
    pub fn clone_service(&self) -> Self {
        self.clone()
    }
}

impl CriService {
    /// Create a service over an engine and an opened store.
    pub fn new(config: Config, engine: Arc<dyn Engine>, store: Arc<Store>) -> Self {
        let (events, _) = broadcast::channel(256);
        let streaming = StreamingState::new(Arc::clone(&engine), Arc::clone(&store));
        Self {
            inner: Arc::new(Inner {
                pod_cidr: RwLock::new(config.pod_cidr.clone()),
                config,
                engine,
                store,
                events,
                logs: RwLock::new(HashMap::new()),
                streaming,
            }),
        }
    }

    /// Open the store from the config and create a service.
    pub fn open(config: Config, engine: Arc<dyn Engine>) -> CrateResult<Self> {
        let store = Arc::new(Store::open(
            config.sandboxes_dir(),
            config.containers_dir(),
        )?);
        Ok(Self::new(config, engine, store))
    }

    /// Streaming state, so the server can bind the streaming listener.
    pub fn streaming_state(&self) -> Arc<StreamingState> {
        Arc::clone(&self.inner.streaming)
    }

    /// The service configuration.
    pub fn config(&self) -> &Config {
        &self.inner.config
    }
}

impl Inner {
    fn emit(&self, container_id: &str, event: ContainerEventType) {
        let sandbox_status = self
            .store
            .container(container_id)
            .and_then(|c| self.store.sandbox(&c.sandbox_id))
            .map(|s| sandbox_status(&s));
        let containers_statuses = self
            .store
            .container(container_id)
            .map(|c| vec![container_status(&c)])
            .unwrap_or_default();
        let _ = self.events.send(ContainerEventResponse {
            container_id: container_id.to_string(),
            container_event_type: event as i32,
            created_at: now_ns(),
            pod_sandbox_status: sandbox_status,
            containers_statuses,
        });
    }

    fn sandbox_record(&self, id: &str) -> CrateResult<SandboxRecord> {
        self.store
            .sandbox(id)
            .ok_or_else(|| Error::new(ErrorCode::NotFound, format!("sandbox {id} not found"), ""))
    }

    fn container_record(&self, id: &str) -> CrateResult<ContainerRecord> {
        self.store
            .container(id)
            .ok_or_else(|| Error::new(ErrorCode::NotFound, format!("container {id} not found"), ""))
    }

    /// Reconcile a record with the engine (a container may have exited
    /// without the monitor noticing, e.g. across a daemon restart).
    fn reconcile(&self, mut record: ContainerRecord) -> ContainerRecord {
        if record.state == StoredState::Running {
            if let Ok(state) = self.engine.container_state(&record.id) {
                if state.status == "stopped" || state.status == "created" {
                    record.state = if state.status == "created" {
                        StoredState::Created
                    } else {
                        StoredState::Exited
                    };
                    if state.exit_code >= 0 {
                        record.exit_code = state.exit_code;
                        record.reason = exit_reason(state.exit_code, state.oom_killed).into();
                    }
                    if record.finished_at_ns == 0 && record.state == StoredState::Exited {
                        record.finished_at_ns = now_ns();
                    }
                    let _ = self.store.put_container(record.clone());
                }
            }
        }
        record
    }

    fn find_image(&self, requested: &str) -> CrateResult<EngineImage> {
        let images = self.engine.list_images("")?;
        images
            .into_iter()
            .find(|img| img.matches_reference(requested))
            .ok_or_else(|| {
                Error::new(
                    ErrorCode::NotFound,
                    format!("image {requested} is not present; pull it first"),
                    "",
                )
            })
    }

    fn ensure_image(&self, requested: &str) -> CrateResult<EngineImage> {
        match self.find_image(requested) {
            Ok(image) => Ok(image),
            Err(_) => {
                let image = crate::image_reference::normalize(requested)?;
                self.engine
                    .pull_image(&image, None, self.config.insecure_registries)?;
                self.find_image(requested)
            }
        }
    }

    fn namespace_paths(&self, sandbox: &SandboxRecord) -> CrateResult<NamespacePaths> {
        let state = self.engine.container_state(&sandbox.infra_container_id)?;
        if state.pid <= 0 {
            return Ok(NamespacePaths::default());
        }
        let base = format!("/proc/{}/ns", state.pid);
        let path = |name: &str| {
            Some(
                state
                    .namespaces
                    .get(name)
                    .cloned()
                    .unwrap_or_else(|| format!("{base}/{name}")),
            )
        };
        Ok(NamespacePaths {
            network: path("net"),
            ipc: path("ipc"),
            uts: path("uts"),
            pid: path("pid"),
            user: None,
        })
    }

    fn spawn_monitor(&self, container_id: String) {
        let inner_engine = Arc::clone(&self.engine);
        let store = Arc::clone(&self.store);
        let inner = self.events.clone();
        std::thread::Builder::new()
            .name(format!("rok-wait-{container_id}"))
            .spawn(move || {
                let code = inner_engine.wait_container(&container_id).unwrap_or(-1);
                if let Some(mut record) = store.container(&container_id) {
                    record.state = StoredState::Exited;
                    record.exit_code = code;
                    record.finished_at_ns = now_ns();
                    let oom = inner_engine
                        .container_state(&container_id)
                        .is_ok_and(|s| s.oom_killed);
                    record.reason = exit_reason(code, oom).into();
                    let _ = store.put_container(record);
                }
                let _ = inner.send(ContainerEventResponse {
                    container_id: container_id.clone(),
                    container_event_type: ContainerEventType::ContainerStoppedEvent as i32,
                    created_at: now_ns(),
                    pod_sandbox_status: None,
                    containers_statuses: Vec::new(),
                });
            })
            .ok();
    }
}

fn exit_reason(code: i32, oom_killed: bool) -> &'static str {
    if code == 137 && oom_killed {
        "OOMKilled"
    } else if code == 0 {
        "Completed"
    } else {
        "Error"
    }
}

fn hashmap(map: &BTreeMap<String, String>) -> HashMap<String, String> {
    map.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
}

fn sandbox_metadata(record: &SandboxRecord) -> PodSandboxMetadata {
    PodSandboxMetadata {
        name: record.metadata.name.clone(),
        uid: record.metadata.uid.clone(),
        namespace: record.metadata.namespace.clone(),
        attempt: record.metadata.attempt,
    }
}

fn sandbox_status(record: &SandboxRecord) -> PodSandboxStatus {
    PodSandboxStatus {
        id: record.id.clone(),
        metadata: Some(sandbox_metadata(record)),
        state: match record.state {
            SandboxState::Ready => PodSandboxState::SandboxReady as i32,
            SandboxState::NotReady => PodSandboxState::SandboxNotready as i32,
        },
        created_at: record.created_at_ns,
        network: Some(PodSandboxNetworkStatus {
            ip: record.ip.clone(),
            additional_ips: record
                .extra_ips
                .iter()
                .map(|ip| PodIp { ip: ip.clone() })
                .collect(),
        }),
        linux: Some(LinuxPodSandboxStatus {
            namespaces: Some(Namespace {
                options: Some(NamespaceOption {
                    network: if record.host_network {
                        NamespaceMode::Node as i32
                    } else {
                        NamespaceMode::Pod as i32
                    },
                    ..NamespaceOption::default()
                }),
            }),
        }),
        labels: hashmap(&record.labels),
        annotations: hashmap(&record.annotations),
        runtime_handler: String::new(),
    }
}

fn container_metadata(record: &ContainerRecord) -> ContainerMetadata {
    ContainerMetadata {
        name: record.metadata.name.clone(),
        attempt: record.metadata.attempt,
    }
}

fn container_status(record: &ContainerRecord) -> ContainerStatus {
    let config = ContainerConfig::decode(record.config_bytes.as_slice()).unwrap_or_default();
    ContainerStatus {
        id: record.id.clone(),
        metadata: Some(container_metadata(record)),
        state: match record.state {
            StoredState::Created => ContainerState::ContainerCreated as i32,
            StoredState::Running => ContainerState::ContainerRunning as i32,
            StoredState::Exited => ContainerState::ContainerExited as i32,
            StoredState::Unknown => ContainerState::ContainerUnknown as i32,
        },
        created_at: record.created_at_ns,
        started_at: record.started_at_ns,
        finished_at: record.finished_at_ns,
        exit_code: record.exit_code,
        image: Some(ImageSpec {
            image: record.image.clone(),
            annotations: HashMap::new(),
            user_specified_image: record.image.clone(),
            runtime_handler: String::new(),
            image_ref: String::new(),
        }),
        image_ref: record.image_ref.clone(),
        reason: record.reason.clone(),
        message: record.message.clone(),
        labels: hashmap(&record.labels),
        annotations: hashmap(&record.annotations),
        mounts: config.mounts,
        log_path: record.log_path.clone(),
        resources: None,
        image_id: String::new(),
        user: serde_json::from_str::<serde_json::Value>(&record.spec_json)
            .ok()
            .and_then(|spec| {
                serde_json::from_value::<crate::engine::ResolvedUser>(
                    spec["process"]["user"].clone(),
                )
                .ok()
            })
            .map(|user| ContainerUser {
                linux: Some(LinuxContainerUser {
                    uid: i64::from(user.uid),
                    gid: i64::from(user.gid),
                    supplemental_groups: user.additional_gids.into_iter().map(i64::from).collect(),
                }),
            }),
        stop_signal: signal_number(&record.stop_signal),
    }
}

fn container_summary(record: &ContainerRecord) -> Container {
    Container {
        id: record.id.clone(),
        pod_sandbox_id: record.sandbox_id.clone(),
        metadata: Some(container_metadata(record)),
        image: Some(ImageSpec {
            image: record.image.clone(),
            annotations: HashMap::new(),
            user_specified_image: record.image.clone(),
            runtime_handler: String::new(),
            image_ref: String::new(),
        }),
        image_ref: record.image_ref.clone(),
        state: container_status(record).state,
        created_at: record.created_at_ns,
        labels: hashmap(&record.labels),
        annotations: hashmap(&record.annotations),
        image_id: String::new(),
    }
}

/// Map a stop-signal name (`SIGTERM`, `TERM`, `15`) to a signal number.
pub fn signal_number(name: &str) -> i32 {
    let name = name.trim().to_ascii_uppercase();
    if let Ok(n) = name.parse::<i32>() {
        return n;
    }
    match name.strip_prefix("SIG").unwrap_or(&name) {
        "HUP" => 1,
        "INT" => 2,
        "QUIT" => 3,
        "KILL" => 9,
        "USR1" => 10,
        "USR2" => 12,
        "TERM" => 15,
        "STOP" => 19,
        "" => 15,
        _ => 15,
    }
}

fn labels_match(labels: &BTreeMap<String, String>, selector: &HashMap<String, String>) -> bool {
    selector.iter().all(|(k, v)| labels.get(k) == Some(v))
}

#[tonic::async_trait]
impl RuntimeService for CriService {
    async fn version(
        &self,
        _request: Request<VersionRequest>,
    ) -> std::result::Result<Response<VersionResponse>, Status> {
        Ok(Response::new(VersionResponse {
            version: API_VERSION.to_string(),
            runtime_name: self.inner.config.runtime_name.clone(),
            runtime_version: env!("CARGO_PKG_VERSION").to_string(),
            runtime_api_version: API_VERSION.to_string(),
        }))
    }

    async fn run_pod_sandbox(
        &self,
        request: Request<RunPodSandboxRequest>,
    ) -> std::result::Result<Response<RunPodSandboxResponse>, Status> {
        let inner = Arc::clone(&self.inner);
        let request = request.into_inner();
        let id = tokio::task::spawn_blocking(move || run_pod_sandbox_sync(&inner, request))
            .await
            .map_err(|e| Status::internal(e.to_string()))??;
        Ok(Response::new(RunPodSandboxResponse { pod_sandbox_id: id }))
    }

    async fn stop_pod_sandbox(
        &self,
        request: Request<StopPodSandboxRequest>,
    ) -> std::result::Result<Response<StopPodSandboxResponse>, Status> {
        let inner = Arc::clone(&self.inner);
        let id = request.into_inner().pod_sandbox_id;
        tokio::task::spawn_blocking(move || stop_pod_sandbox_sync(&inner, &id))
            .await
            .map_err(|e| Status::internal(e.to_string()))??;
        Ok(Response::new(StopPodSandboxResponse {}))
    }

    async fn remove_pod_sandbox(
        &self,
        request: Request<RemovePodSandboxRequest>,
    ) -> std::result::Result<Response<RemovePodSandboxResponse>, Status> {
        let inner = Arc::clone(&self.inner);
        let id = request.into_inner().pod_sandbox_id;
        tokio::task::spawn_blocking(move || -> CrateResult<()> {
            if inner.store.sandbox(&id).is_none() {
                return Ok(()); // idempotent
            }
            let _ = stop_pod_sandbox_sync(&inner, &id);
            let record = inner.sandbox_record(&id)?;
            let containers = inner.store.containers_in(&id);
            for container in &containers {
                let _ = remove_container_sync(&inner, &container.id);
            }
            let _ = inner.engine.remove_container(&record.infra_container_id);
            let _ = inner.engine.release_rootfs(&record.infra_container_id);
            crate::dns::remove(&inner.config.root, &id)?;
            inner.store.remove_sandbox(&id)
        })
        .await
        .map_err(|e| Status::internal(e.to_string()))??;
        Ok(Response::new(RemovePodSandboxResponse {}))
    }

    async fn pod_sandbox_status(
        &self,
        request: Request<PodSandboxStatusRequest>,
    ) -> std::result::Result<Response<PodSandboxStatusResponse>, Status> {
        let request = request.into_inner();
        let record = self
            .inner
            .sandbox_record(&request.pod_sandbox_id)
            .map_err(Status::from)?;
        let containers_statuses = self
            .inner
            .store
            .containers_in(&record.id)
            .into_iter()
            .map(|c| container_status(&self.inner.reconcile(c)))
            .collect();
        Ok(Response::new(PodSandboxStatusResponse {
            status: Some(sandbox_status(&record)),
            info: if request.verbose {
                HashMap::from([(
                    "config".to_string(),
                    serde_json::to_string(&record.linux_json).unwrap_or_default(),
                )])
            } else {
                HashMap::new()
            },
            containers_statuses,
            timestamp: now_ns(),
        }))
    }

    async fn list_pod_sandbox(
        &self,
        request: Request<ListPodSandboxRequest>,
    ) -> std::result::Result<Response<ListPodSandboxResponse>, Status> {
        let filter = request.into_inner().filter;
        let items = self
            .inner
            .store
            .sandboxes()
            .into_iter()
            .filter(|s| sandbox_matches(s, filter.as_ref()))
            .map(|s| PodSandbox {
                id: s.id.clone(),
                metadata: Some(sandbox_metadata(&s)),
                state: sandbox_status(&s).state,
                created_at: s.created_at_ns,
                labels: hashmap(&s.labels),
                annotations: hashmap(&s.annotations),
                runtime_handler: String::new(),
            })
            .collect();
        Ok(Response::new(ListPodSandboxResponse { items }))
    }

    type StreamPodSandboxesStream = BoxStream<StreamPodSandboxesResponse>;

    async fn stream_pod_sandboxes(
        &self,
        request: Request<StreamPodSandboxesRequest>,
    ) -> std::result::Result<Response<Self::StreamPodSandboxesStream>, Status> {
        let listed = self
            .list_pod_sandbox(Request::new(ListPodSandboxRequest {
                filter: request.into_inner().filter,
            }))
            .await?
            .into_inner();
        let stream = tonic::codegen::tokio_stream::iter(vec![Ok(StreamPodSandboxesResponse {
            pod_sandboxes: listed.items,
        })]);
        Ok(Response::new(Box::pin(stream)))
    }

    async fn create_container(
        &self,
        request: Request<CreateContainerRequest>,
    ) -> std::result::Result<Response<CreateContainerResponse>, Status> {
        let inner = Arc::clone(&self.inner);
        let request = request.into_inner();
        let id = tokio::task::spawn_blocking(move || create_container_sync(&inner, request))
            .await
            .map_err(|e| Status::internal(e.to_string()))??;
        Ok(Response::new(CreateContainerResponse { container_id: id }))
    }

    async fn start_container(
        &self,
        request: Request<StartContainerRequest>,
    ) -> std::result::Result<Response<StartContainerResponse>, Status> {
        let inner = Arc::clone(&self.inner);
        let id = request.into_inner().container_id;
        tokio::task::spawn_blocking(move || start_container_sync(&inner, &id))
            .await
            .map_err(|e| Status::internal(e.to_string()))??;
        Ok(Response::new(StartContainerResponse {}))
    }

    async fn stop_container(
        &self,
        request: Request<StopContainerRequest>,
    ) -> std::result::Result<Response<StopContainerResponse>, Status> {
        let inner = Arc::clone(&self.inner);
        let request = request.into_inner();
        tokio::task::spawn_blocking(move || {
            stop_container_sync(&inner, &request.container_id, request.timeout)
        })
        .await
        .map_err(|e| Status::internal(e.to_string()))??;
        Ok(Response::new(StopContainerResponse {}))
    }

    async fn remove_container(
        &self,
        request: Request<RemoveContainerRequest>,
    ) -> std::result::Result<Response<RemoveContainerResponse>, Status> {
        let inner = Arc::clone(&self.inner);
        let id = request.into_inner().container_id;
        tokio::task::spawn_blocking(move || remove_container_sync(&inner, &id))
            .await
            .map_err(|e| Status::internal(e.to_string()))??;
        Ok(Response::new(RemoveContainerResponse {}))
    }

    async fn list_containers(
        &self,
        request: Request<ListContainersRequest>,
    ) -> std::result::Result<Response<ListContainersResponse>, Status> {
        let filter = request.into_inner().filter;
        let containers = self
            .inner
            .store
            .containers()
            .into_iter()
            .map(|c| self.inner.reconcile(c))
            .filter(|c| container_matches(c, filter.as_ref()))
            .map(|c| container_summary(&c))
            .collect();
        Ok(Response::new(ListContainersResponse { containers }))
    }

    type StreamContainersStream = BoxStream<StreamContainersResponse>;

    async fn stream_containers(
        &self,
        request: Request<StreamContainersRequest>,
    ) -> std::result::Result<Response<Self::StreamContainersStream>, Status> {
        let listed = self
            .list_containers(Request::new(ListContainersRequest {
                filter: request.into_inner().filter,
            }))
            .await?
            .into_inner();
        let stream = tonic::codegen::tokio_stream::iter(vec![Ok(StreamContainersResponse {
            containers: listed.containers,
        })]);
        Ok(Response::new(Box::pin(stream)))
    }

    async fn container_status(
        &self,
        request: Request<ContainerStatusRequest>,
    ) -> std::result::Result<Response<ContainerStatusResponse>, Status> {
        let request = request.into_inner();
        let record = self
            .inner
            .container_record(&request.container_id)
            .map_err(Status::from)?;
        let record = self.inner.reconcile(record);
        Ok(Response::new(ContainerStatusResponse {
            status: Some(container_status(&record)),
            info: if request.verbose {
                HashMap::from([("spec".to_string(), record.spec_json.clone())])
            } else {
                HashMap::new()
            },
        }))
    }

    async fn update_container_resources(
        &self,
        request: Request<UpdateContainerResourcesRequest>,
    ) -> std::result::Result<Response<UpdateContainerResourcesResponse>, Status> {
        let inner = Arc::clone(&self.inner);
        let request = request.into_inner();
        tokio::task::spawn_blocking(move || -> CrateResult<()> {
            let _ = inner.container_record(&request.container_id)?;
            if let Some(linux) = request.linux {
                let resources = convert::resources_json(&linux);
                inner
                    .engine
                    .update_resources(&request.container_id, &resources.to_string())?;
            }
            Ok(())
        })
        .await
        .map_err(|e| Status::internal(e.to_string()))??;
        Ok(Response::new(UpdateContainerResourcesResponse {}))
    }

    async fn reopen_container_log(
        &self,
        request: Request<ReopenContainerLogRequest>,
    ) -> std::result::Result<Response<ReopenContainerLogResponse>, Status> {
        let id = request.into_inner().container_id;
        let _ = self.inner.container_record(&id).map_err(Status::from)?;
        let logs = self.inner.logs.read().unwrap_or_else(|p| p.into_inner());
        if let Some(writer) = logs.get(&id) {
            writer.reopen().map_err(Status::from)?;
        }
        Ok(Response::new(ReopenContainerLogResponse {}))
    }

    async fn exec_sync(
        &self,
        request: Request<ExecSyncRequest>,
    ) -> std::result::Result<Response<ExecSyncResponse>, Status> {
        let inner = Arc::clone(&self.inner);
        let request = request.into_inner();
        let output = tokio::task::spawn_blocking(move || {
            inner
                .engine
                .exec_sync(&request.container_id, &request.cmd, request.timeout)
        })
        .await
        .map_err(|e| Status::internal(e.to_string()))?
        .map_err(Status::from)?;
        Ok(Response::new(ExecSyncResponse {
            stdout: output.stdout,
            stderr: output.stderr,
            exit_code: output.exit_code,
        }))
    }

    async fn exec(
        &self,
        request: Request<ExecRequest>,
    ) -> std::result::Result<Response<ExecResponse>, Status> {
        let request = request.into_inner();
        let _ = self
            .inner
            .container_record(&request.container_id)
            .map_err(Status::from)?;
        let url = self.inner.streaming.url(
            "exec",
            SessionKind::Exec {
                container_id: request.container_id,
                cmd: request.cmd,
                tty: request.tty,
                stdin: request.stdin,
                stdout: request.stdout,
                stderr: request.stderr,
            },
        );
        Ok(Response::new(ExecResponse { url }))
    }

    async fn attach(
        &self,
        request: Request<AttachRequest>,
    ) -> std::result::Result<Response<AttachResponse>, Status> {
        let request = request.into_inner();
        let record = self
            .inner
            .container_record(&request.container_id)
            .map_err(Status::from)?;
        let url = self.inner.streaming.url(
            "attach",
            SessionKind::Attach {
                container_id: request.container_id,
                stdin: request.stdin,
                stdin_once: record.stdin_once,
                tty: request.tty,
                stdout: request.stdout,
                stderr: request.stderr,
            },
        );
        Ok(Response::new(AttachResponse { url }))
    }

    async fn port_forward(
        &self,
        request: Request<PortForwardRequest>,
    ) -> std::result::Result<Response<PortForwardResponse>, Status> {
        let request = request.into_inner();
        let sandbox = self
            .inner
            .sandbox_record(&request.pod_sandbox_id)
            .map_err(Status::from)?;
        let url = self.inner.streaming.url(
            "portforward",
            SessionKind::PortForward {
                pod_sandbox_id: sandbox.id.clone(),
                ip: if sandbox.ip.is_empty() {
                    "127.0.0.1".to_string()
                } else {
                    sandbox.ip.clone()
                },
            },
        );
        Ok(Response::new(PortForwardResponse { url }))
    }

    async fn container_stats(
        &self,
        request: Request<ContainerStatsRequest>,
    ) -> std::result::Result<Response<ContainerStatsResponse>, Status> {
        let inner = Arc::clone(&self.inner);
        let id = request.into_inner().container_id;
        let stats = tokio::task::spawn_blocking(move || container_stats_sync(&inner, &id))
            .await
            .map_err(|e| Status::internal(e.to_string()))??;
        Ok(Response::new(ContainerStatsResponse { stats: Some(stats) }))
    }

    async fn list_container_stats(
        &self,
        request: Request<ListContainerStatsRequest>,
    ) -> std::result::Result<Response<ListContainerStatsResponse>, Status> {
        let filter = request.into_inner().filter;
        let mut stats = Vec::new();
        for record in self.inner.store.containers() {
            if !stats_container_matches(&record, filter.as_ref()) {
                continue;
            }
            match self.inner.engine.container_stats(&record.id) {
                Ok(one) => stats.push(build_container_stats(&record, &one)),
                Err(error) => {
                    eprintln!(
                        "oci-cri: failed to collect container {} stats: {error}",
                        record.id
                    )
                }
            }
        }
        Ok(Response::new(ListContainerStatsResponse { stats }))
    }

    type StreamContainerStatsStream = BoxStream<StreamContainerStatsResponse>;

    async fn stream_container_stats(
        &self,
        request: Request<StreamContainerStatsRequest>,
    ) -> std::result::Result<Response<Self::StreamContainerStatsStream>, Status> {
        let listed = self
            .list_container_stats(Request::new(ListContainerStatsRequest {
                filter: request.into_inner().filter,
            }))
            .await?
            .into_inner();
        let stream = tonic::codegen::tokio_stream::iter(vec![Ok(StreamContainerStatsResponse {
            container_stats: listed.stats,
        })]);
        Ok(Response::new(Box::pin(stream)))
    }

    async fn pod_sandbox_stats(
        &self,
        request: Request<PodSandboxStatsRequest>,
    ) -> std::result::Result<Response<PodSandboxStatsResponse>, Status> {
        let inner = Arc::clone(&self.inner);
        let id = request.into_inner().pod_sandbox_id;
        let stats = tokio::task::spawn_blocking(move || pod_sandbox_stats_sync(&inner, &id))
            .await
            .map_err(|e| Status::internal(e.to_string()))??;
        Ok(Response::new(PodSandboxStatsResponse {
            stats: Some(stats),
        }))
    }

    async fn list_pod_sandbox_stats(
        &self,
        request: Request<ListPodSandboxStatsRequest>,
    ) -> std::result::Result<Response<ListPodSandboxStatsResponse>, Status> {
        let filter = request.into_inner().filter;
        let mut stats = Vec::new();
        for sandbox in self.inner.store.sandboxes() {
            if let Some(filter) = &filter {
                if !filter.id.is_empty() && filter.id != sandbox.id {
                    continue;
                }
                if !labels_match(&sandbox.labels, &filter.label_selector) {
                    continue;
                }
            }
            match pod_sandbox_stats_sync(&self.inner, &sandbox.id) {
                Ok(one) => stats.push(one),
                Err(error) => {
                    eprintln!(
                        "oci-cri: failed to collect sandbox {} stats: {error}",
                        sandbox.id
                    )
                }
            }
        }
        Ok(Response::new(ListPodSandboxStatsResponse { stats }))
    }

    type StreamPodSandboxStatsStream = BoxStream<StreamPodSandboxStatsResponse>;

    async fn stream_pod_sandbox_stats(
        &self,
        request: Request<StreamPodSandboxStatsRequest>,
    ) -> std::result::Result<Response<Self::StreamPodSandboxStatsStream>, Status> {
        let listed = self
            .list_pod_sandbox_stats(Request::new(ListPodSandboxStatsRequest {
                filter: request.into_inner().filter,
            }))
            .await?
            .into_inner();
        let stream = tonic::codegen::tokio_stream::iter(vec![Ok(StreamPodSandboxStatsResponse {
            pod_sandbox_stats: listed.stats,
        })]);
        Ok(Response::new(Box::pin(stream)))
    }

    async fn update_runtime_config(
        &self,
        request: Request<UpdateRuntimeConfigRequest>,
    ) -> std::result::Result<Response<UpdateRuntimeConfigResponse>, Status> {
        if let Some(config) = request.into_inner().runtime_config {
            if let Some(network) = config.network_config {
                *self
                    .inner
                    .pod_cidr
                    .write()
                    .unwrap_or_else(|p| p.into_inner()) = network.pod_cidr;
            }
        }
        Ok(Response::new(UpdateRuntimeConfigResponse {}))
    }

    async fn status(
        &self,
        request: Request<StatusRequest>,
    ) -> std::result::Result<Response<StatusResponse>, Status> {
        let verbose = request.into_inner().verbose;
        let network_ready =
            !network::configured(&self.inner.config) || self.inner.config.cni_conf_dir.is_some();
        Ok(Response::new(StatusResponse {
            status: Some(RuntimeStatus {
                conditions: vec![
                    RuntimeCondition {
                        r#type: "RuntimeReady".to_string(),
                        status: true,
                        reason: String::new(),
                        message: String::new(),
                    },
                    RuntimeCondition {
                        r#type: "NetworkReady".to_string(),
                        status: network_ready,
                        reason: if network_ready {
                            String::new()
                        } else {
                            "NetworkPluginNotReady".to_string()
                        },
                        message: String::new(),
                    },
                ],
            }),
            info: if verbose {
                HashMap::from([
                    (
                        "runtime".to_string(),
                        self.inner.config.runtime_name.clone(),
                    ),
                    (
                        "podCIDR".to_string(),
                        self.inner
                            .pod_cidr
                            .read()
                            .unwrap_or_else(|p| p.into_inner())
                            .clone(),
                    ),
                ])
            } else {
                HashMap::new()
            },
            runtime_handlers: vec![RuntimeHandler {
                name: String::new(),
                features: Some(RuntimeHandlerFeatures {
                    recursive_read_only_mounts: true,
                    user_namespaces: false,
                }),
            }],
            features: Some(RuntimeFeatures {
                supplemental_groups_policy: true,
                user_namespaces_host_network: false,
                mount_options: false,
            }),
        }))
    }

    async fn checkpoint_container(
        &self,
        request: Request<CheckpointContainerRequest>,
    ) -> std::result::Result<Response<CheckpointContainerResponse>, Status> {
        let inner = Arc::clone(&self.inner);
        let request = request.into_inner();
        tokio::task::spawn_blocking(move || -> CrateResult<()> {
            let _ = inner.container_record(&request.container_id)?;
            inner
                .engine
                .checkpoint(&request.container_id, &request.location, "")
        })
        .await
        .map_err(|e| Status::internal(e.to_string()))??;
        Ok(Response::new(CheckpointContainerResponse {}))
    }

    async fn checkpoint_pod(
        &self,
        request: Request<CheckpointPodRequest>,
    ) -> std::result::Result<Response<CheckpointPodResponse>, Status> {
        let inner = Arc::clone(&self.inner);
        let request = request.into_inner();
        tokio::task::spawn_blocking(move || -> CrateResult<()> {
            let sandbox = inner.sandbox_record(&request.pod_sandbox_id)?;
            for container in inner.store.containers_in(&sandbox.id) {
                if container.state == StoredState::Running {
                    let location = format!("{}/{}", request.output_path, container.id);
                    inner.engine.checkpoint(&container.id, &location, "")?;
                }
            }
            Ok(())
        })
        .await
        .map_err(|e| Status::internal(e.to_string()))??;
        Ok(Response::new(CheckpointPodResponse {}))
    }

    async fn restore_pod(
        &self,
        _request: Request<RestorePodRequest>,
    ) -> std::result::Result<Response<RestorePodResponse>, Status> {
        // Pod restore needs sandbox state serialised at checkpoint time,
        // which CRI checkpoints do not carry; container restore is reached
        // through CheckpointContainer + CreateContainer on the target node.
        Err(Status::unimplemented(
            "pod restore is not supported; restore containers individually",
        ))
    }

    type GetContainerEventsStream = BoxStream<ContainerEventResponse>;

    async fn get_container_events(
        &self,
        _request: Request<GetEventsRequest>,
    ) -> std::result::Result<Response<Self::GetContainerEventsStream>, Status> {
        let rx = self.inner.events.subscribe();
        let stream = tokio_stream::wrappers::BroadcastStream::new(rx);
        let stream = futures_util::StreamExt::filter_map(stream, |item| async move { item.ok() });
        let stream = futures_util::StreamExt::map(stream, Ok);
        Ok(Response::new(Box::pin(stream)))
    }

    async fn list_metric_descriptors(
        &self,
        _request: Request<ListMetricDescriptorsRequest>,
    ) -> std::result::Result<Response<ListMetricDescriptorsResponse>, Status> {
        Ok(Response::new(ListMetricDescriptorsResponse {
            descriptors: metric_descriptors(),
        }))
    }

    async fn list_pod_sandbox_metrics(
        &self,
        _request: Request<ListPodSandboxMetricsRequest>,
    ) -> std::result::Result<Response<ListPodSandboxMetricsResponse>, Status> {
        let mut pod_metrics = Vec::new();
        for sandbox in self.inner.store.sandboxes() {
            let mut metrics = Vec::new();
            let mut container_metrics = Vec::new();
            for record in self.inner.store.containers_in(&sandbox.id) {
                if let Ok(raw) = self.inner.engine.container_stats(&record.id) {
                    let list = stats_metrics(&raw);
                    if record.id == sandbox.infra_container_id {
                        metrics = list.clone();
                    }
                    container_metrics.push(ContainerMetrics {
                        container_id: record.id.clone(),
                        metrics: list,
                    });
                }
            }
            pod_metrics.push(PodSandboxMetrics {
                pod_sandbox_id: sandbox.id.clone(),
                metrics,
                container_metrics,
            });
        }
        Ok(Response::new(ListPodSandboxMetricsResponse { pod_metrics }))
    }

    type StreamPodSandboxMetricsStream = BoxStream<StreamPodSandboxMetricsResponse>;

    async fn stream_pod_sandbox_metrics(
        &self,
        _request: Request<StreamPodSandboxMetricsRequest>,
    ) -> std::result::Result<Response<Self::StreamPodSandboxMetricsStream>, Status> {
        let listed = self
            .list_pod_sandbox_metrics(Request::new(ListPodSandboxMetricsRequest {}))
            .await?
            .into_inner();
        let stream =
            tonic::codegen::tokio_stream::iter(vec![Ok(StreamPodSandboxMetricsResponse {
                pod_sandbox_metrics: listed.pod_metrics,
            })]);
        Ok(Response::new(Box::pin(stream)))
    }

    async fn runtime_config(
        &self,
        _request: Request<RuntimeConfigRequest>,
    ) -> std::result::Result<Response<RuntimeConfigResponse>, Status> {
        Ok(Response::new(RuntimeConfigResponse {
            linux: Some(LinuxRuntimeConfiguration {
                cgroup_driver: CgroupDriver::Cgroupfs as i32,
            }),
        }))
    }

    async fn update_pod_sandbox_resources(
        &self,
        request: Request<UpdatePodSandboxResourcesRequest>,
    ) -> std::result::Result<Response<UpdatePodSandboxResourcesResponse>, Status> {
        let inner = Arc::clone(&self.inner);
        let request = request.into_inner();
        tokio::task::spawn_blocking(move || -> CrateResult<()> {
            let sandbox = inner.sandbox_record(&request.pod_sandbox_id)?;
            if let Some(linux) = request.resources {
                let resources = serde_json::json!({
                    "cpu": {
                        "shares": linux.cpu_shares,
                        "period": linux.cpu_period,
                        "quota": linux.cpu_quota,
                        "cpus": linux.cpuset_cpus,
                        "mems": linux.cpuset_mems,
                    },
                    "memory": { "limit": linux.memory_limit_in_bytes },
                });
                let _ = inner
                    .engine
                    .update_resources(&sandbox.infra_container_id, &resources.to_string());
            }
            Ok(())
        })
        .await
        .map_err(|e| Status::internal(e.to_string()))??;
        Ok(Response::new(UpdatePodSandboxResourcesResponse {}))
    }
}

fn sandbox_matches(record: &SandboxRecord, filter: Option<&PodSandboxFilter>) -> bool {
    let Some(filter) = filter else { return true };
    if !filter.id.is_empty() && filter.id != record.id {
        return false;
    }
    if let Some(state) = &filter.state {
        let want = state.state == PodSandboxState::SandboxReady as i32;
        if want != (record.state == SandboxState::Ready) {
            return false;
        }
    }
    labels_match(&record.labels, &filter.label_selector)
}

fn container_matches(record: &ContainerRecord, filter: Option<&ContainerFilter>) -> bool {
    let Some(filter) = filter else { return true };
    if !filter.id.is_empty() && filter.id != record.id {
        return false;
    }
    if !filter.pod_sandbox_id.is_empty() && filter.pod_sandbox_id != record.sandbox_id {
        return false;
    }
    if let Some(state) = &filter.state {
        if state.state != container_status(record).state {
            return false;
        }
    }
    labels_match(&record.labels, &filter.label_selector)
}

fn stats_container_matches(
    record: &ContainerRecord,
    filter: Option<&ContainerStatsFilter>,
) -> bool {
    let Some(filter) = filter else { return true };
    if !filter.id.is_empty() && filter.id != record.id {
        return false;
    }
    if !filter.pod_sandbox_id.is_empty() && filter.pod_sandbox_id != record.sandbox_id {
        return false;
    }
    labels_match(&record.labels, &filter.label_selector)
}

fn sandbox_hostname(config: &PodSandboxConfig) -> CrateResult<String> {
    if config.hostname.contains('\0') || config.hostname.len() > 64 {
        return Err(Error::new(
            ErrorCode::InvalidArgument,
            "hostname must be at most 64 bytes and contain no NUL",
            "",
        ));
    }
    if !config.hostname.is_empty() {
        return Ok(config.hostname.clone());
    }
    // CRI metadata names are not hostnames and may exceed Linux's UTS limit.
    let name = config
        .metadata
        .as_ref()
        .map(|m| m.name.as_str())
        .unwrap_or_default();
    let mut end = name.len().min(64);
    while !name.is_char_boundary(end) {
        end -= 1;
    }
    let hostname = &name[..end];
    if hostname.contains('\0') {
        return Err(Error::new(
            ErrorCode::InvalidArgument,
            "sandbox name contains NUL",
            "",
        ));
    }
    Ok(hostname.to_string())
}

pub(crate) fn run_pod_sandbox_sync(
    inner: &Inner,
    request: RunPodSandboxRequest,
) -> CrateResult<String> {
    let config = request
        .config
        .ok_or_else(|| Error::new(ErrorCode::InvalidArgument, "sandbox config is missing", ""))?;
    let hostname = sandbox_hostname(&config)?;
    let metadata = config.metadata.clone().unwrap_or_default();
    let sandbox_id = new_id();
    let infra_id = new_id();
    let image = inner.ensure_image(&inner.config.sandbox_image)?;
    let rootfs = inner.engine.prepare_rootfs(&infra_id, &image.id)?;
    let mut resolver =
        crate::dns::Resolver::create(&inner.config.root, &sandbox_id, config.dns_config.as_ref())?;
    let spec = convert::sandbox_spec(
        &config,
        &rootfs.to_string_lossy(),
        image.spec.as_ref(),
        &hostname,
    )?;
    let spec = crate::dns::mount(spec, &inner.config.root, &sandbox_id)?;
    inner.engine.create_container(&infra_id, &spec)?;
    let host_network = config
        .linux
        .as_ref()
        .and_then(|l| l.security_context.as_ref())
        .and_then(|s| s.namespace_options.as_ref())
        .map(|ns| ns.network == NamespaceMode::Node as i32)
        .unwrap_or(false);
    let log_writer: crate::engine::OutputSink = Arc::new(|_, _| {});
    inner.engine.start_container(&infra_id, log_writer)?;
    let mut record = SandboxRecord {
        id: sandbox_id.clone(),
        metadata: StoredMetadata {
            name: metadata.name.clone(),
            uid: metadata.uid.clone(),
            namespace: metadata.namespace.clone(),
            attempt: metadata.attempt,
        },
        labels: config
            .labels
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
        annotations: config
            .annotations
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
        created_at_ns: now_ns(),
        state: SandboxState::Ready,
        infra_container_id: infra_id.clone(),
        ip: String::new(),
        extra_ips: Vec::new(),
        host_network,
        linux_json: format!("{:?}", config.linux),
        log_directory: config.log_directory.clone(),
    };
    if !host_network && network::configured(&inner.config) {
        let state = inner.engine.container_state(&infra_id)?;
        if state.pid > 0 {
            let netns = PathBuf::from(
                state
                    .namespaces
                    .get("net")
                    .cloned()
                    .unwrap_or_else(|| format!("/proc/{}/ns/net", state.pid)),
            );
            match network::setup(&inner.config, &sandbox_id, &netns, &config.port_mappings) {
                Ok(result) => {
                    let mut ips = result.ips.into_iter();
                    if let Some(first) = ips.next() {
                        record.ip = strip_cidr(&first).to_string();
                        record.extra_ips = ips.map(|ip| strip_cidr(&ip).to_string()).collect();
                    }
                }
                Err(err) => {
                    network::teardown(&inner.config, &sandbox_id, &netns);
                    let _ = inner.engine.signal_container(&infra_id, 9);
                    let _ = inner.engine.remove_container(&infra_id);
                    let _ = inner.engine.release_rootfs(&infra_id);
                    return Err(err);
                }
            }
        }
    }
    inner.store.put_sandbox(record)?;
    resolver.commit();
    inner.spawn_monitor(infra_id);
    Ok(sandbox_id)
}

fn strip_cidr(ip: &str) -> &str {
    ip.split('/').next().unwrap_or(ip)
}

pub(crate) fn stop_pod_sandbox_sync(inner: &Inner, id: &str) -> CrateResult<()> {
    let Some(mut record) = inner.store.sandbox(id) else {
        return Ok(()); // idempotent per CRI spec
    };
    for container in inner.store.containers_in(id) {
        if container.state == StoredState::Running {
            let _ = stop_container_sync(inner, &container.id, 2);
        }
    }
    if !record.host_network && network::configured(&inner.config) {
        // DEL must run in the sandbox namespace while its infra process still
        // holds it open. Never substitute the runtime's own namespace.
        let netns = inner
            .engine
            .container_state(&record.infra_container_id)
            .ok()
            .filter(|state| state.pid > 0 && state.status != "stopped")
            .map(|state| {
                PathBuf::from(
                    state
                        .namespaces
                        .get("net")
                        .cloned()
                        .unwrap_or_else(|| format!("/proc/{}/ns/net", state.pid)),
                )
            })
            .unwrap_or_default();
        network::teardown(&inner.config, id, &netns);
    }
    let _ = inner
        .engine
        .signal_container(&record.infra_container_id, 15);
    let state = inner.engine.container_state(&record.infra_container_id);
    if let Ok(state) = state {
        if state.status == "running" {
            let _ = inner.engine.signal_container(&record.infra_container_id, 9);
        }
    }
    record.state = SandboxState::NotReady;
    inner.store.put_sandbox(record)?;
    Ok(())
}

pub(crate) fn create_container_sync(
    inner: &Inner,
    request: CreateContainerRequest,
) -> CrateResult<String> {
    let sandbox = inner.sandbox_record(&request.pod_sandbox_id)?;
    if sandbox.state != SandboxState::Ready {
        return Err(Error::new(
            ErrorCode::State,
            format!("sandbox {} is not ready", sandbox.id),
            "",
        ));
    }
    let config = request.config.ok_or_else(|| {
        Error::new(
            ErrorCode::InvalidArgument,
            "container config is missing",
            "",
        )
    })?;
    let image_spec = config
        .image
        .as_ref()
        .ok_or_else(|| Error::new(ErrorCode::InvalidArgument, "container image is missing", ""))?;
    let image = inner.find_image(&image_spec.image)?;
    let id = new_id();
    let rootfs = inner.engine.prepare_rootfs(&id, &image.id)?;
    let spec_json = (|| -> CrateResult<String> {
        let mut namespace_paths = inner.namespace_paths(&sandbox)?;
        // Per-container namespace modes: NODE means host namespace (no join).
        if let Some(security) = config
            .linux
            .as_ref()
            .and_then(|l| l.security_context.as_ref())
        {
            if let Some(options) = &security.namespace_options {
                if options.network == NamespaceMode::Node as i32 {
                    namespace_paths.network = None;
                }
                if options.ipc == NamespaceMode::Node as i32 {
                    namespace_paths.ipc = None;
                }
                if options.pid == NamespaceMode::Node as i32 {
                    namespace_paths.pid = None;
                }
            }
        }
        let hostname = sandbox.metadata.name.clone();
        let spec_json = convert::container_spec(
            &config,
            &rootfs.to_string_lossy(),
            image.spec.as_ref(),
            &namespace_paths,
            &hostname,
        )?;
        let (selected, strict) = convert::user_selection(&config, image.spec.as_ref())?;
        let groups: Vec<u32> = config
            .linux
            .as_ref()
            .and_then(|l| l.security_context.as_ref())
            .map(|s| s.supplemental_groups.iter().map(|g| *g as u32).collect())
            .unwrap_or_default();
        let user =
            inner
                .engine
                .resolve_user(&rootfs.to_string_lossy(), &selected, &groups, strict)?;
        let mut spec: serde_json::Value = serde_json::from_str(&spec_json)
            .map_err(|e| Error::new(ErrorCode::Internal, e.to_string(), ""))?;
        spec["process"]["user"] = serde_json::to_value(user)
            .map_err(|e| Error::new(ErrorCode::Internal, e.to_string(), ""))?;
        crate::dns::mount(spec.to_string(), &inner.config.root, &sandbox.id)
    })();
    let spec_json = match spec_json {
        Ok(spec) => spec,
        Err(err) => {
            let _ = inner.engine.release_rootfs(&id);
            return Err(err);
        }
    };

    if let Err(err) = inner.engine.create_container(&id, &spec_json) {
        let _ = inner.engine.release_rootfs(&id);
        return Err(err);
    }
    let metadata = config.metadata.clone().unwrap_or_default();
    let log_path = if config.log_path.is_empty() {
        // Attach still needs output when CRI clients omit a kubelet log path.
        inner
            .config
            .root
            .join("logs")
            .join(format!("{id}.log"))
            .to_string_lossy()
            .into_owned()
    } else {
        format!(
            "{}/{}",
            sandbox.log_directory.trim_end_matches('/'),
            config.log_path
        )
    };
    let record = ContainerRecord {
        id: id.clone(),
        sandbox_id: sandbox.id.clone(),
        metadata: StoredMetadata {
            name: metadata.name.clone(),
            uid: String::new(),
            namespace: String::new(),
            attempt: metadata.attempt,
        },
        image: image_spec.image.clone(),
        image_ref: image
            .repo_digests
            .first()
            .cloned()
            .unwrap_or_else(|| image_spec.image.clone()),
        labels: config
            .labels
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
        annotations: config
            .annotations
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
        log_path,
        tty: config.tty,
        stdin: config.stdin,
        stdin_once: config.stdin_once,
        state: StoredState::Created,
        created_at_ns: now_ns(),
        started_at_ns: 0,
        finished_at_ns: 0,
        exit_code: -1,
        reason: String::new(),
        message: String::new(),
        spec_json,
        config_bytes: config.encode_to_vec(),
        rootfs: rootfs.to_string_lossy().into_owned(),
        stop_signal: image
            .spec
            .as_ref()
            .map(|s| s.stop_signal.clone())
            .unwrap_or_default(),
    };
    inner.store.put_container(record)?;
    inner.emit(&id, ContainerEventType::ContainerCreatedEvent);
    Ok(id)
}

pub(crate) fn start_container_sync(inner: &Inner, id: &str) -> CrateResult<()> {
    let mut record = inner.container_record(id)?;
    if record.state == StoredState::Running {
        return Err(Error::new(
            ErrorCode::State,
            format!("container {id} is already running"),
            "",
        ));
    }
    let sink: crate::engine::OutputSink = if record.log_path.is_empty() {
        Arc::new(|_, _| {})
    } else {
        let writer = CriLogWriter::open(&PathBuf::from(&record.log_path))?;
        let writer = Arc::new(writer);
        inner
            .logs
            .write()
            .unwrap_or_else(|p| p.into_inner())
            .insert(id.to_string(), Arc::clone(&writer));
        Arc::new(move |stream, data| writer.write(stream, data))
    };
    inner.engine.start_container(id, sink)?;
    record.state = StoredState::Running;
    record.started_at_ns = now_ns();
    record.finished_at_ns = 0;
    record.exit_code = -1;
    inner.store.put_container(record)?;
    inner.spawn_monitor(id.to_string());
    inner.emit(id, ContainerEventType::ContainerStartedEvent);
    Ok(())
}

pub(crate) fn stop_container_sync(inner: &Inner, id: &str, timeout_secs: i64) -> CrateResult<()> {
    let record = match inner.container_record(id) {
        Ok(record) => record,
        Err(err) if err.code() == ErrorCode::NotFound => return Ok(()),
        Err(err) => return Err(err),
    };
    if record.state != StoredState::Running {
        return Ok(()); // idempotent
    }
    let signal = if record.stop_signal.is_empty() {
        15
    } else {
        signal_number(&record.stop_signal)
    };
    if timeout_secs <= 0 {
        inner.engine.signal_container(id, 9)?;
    } else {
        inner.engine.signal_container(id, signal)?;
        let deadline = std::time::Instant::now() + Duration::from_secs(timeout_secs as u64);
        loop {
            let current = inner.store.container(id);
            match current {
                Some(c) if c.state != StoredState::Running => break,
                _ => {
                    if std::time::Instant::now() >= deadline {
                        inner.engine.signal_container(id, 9)?;
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(25));
                }
            }
        }
    }
    // Give the monitor a moment to record the exit, then finalise here if it
    // has not (the engine state is authoritative).
    for _ in 0..80 {
        match inner.store.container(id) {
            Some(c) if c.state != StoredState::Running => break,
            _ => std::thread::sleep(Duration::from_millis(25)),
        }
    }
    if let Ok(state) = inner.engine.container_state(id) {
        if state.status == "stopped" {
            if let Some(mut record) = inner.store.container(id) {
                if record.state == StoredState::Running {
                    record.state = StoredState::Exited;
                    record.exit_code = state.exit_code;
                    record.finished_at_ns = now_ns();
                    record.reason = exit_reason(record.exit_code, state.oom_killed).into();
                    let _ = inner.store.put_container(record);
                }
            }
        }
    }
    if let Some(writer) = inner.logs.read().unwrap_or_else(|p| p.into_inner()).get(id) {
        writer.flush_partial();
    }
    Ok(())
}

pub(crate) fn remove_container_sync(inner: &Inner, id: &str) -> CrateResult<()> {
    let Some(record) = inner.store.container(id) else {
        return Ok(()); // idempotent
    };
    if record.state == StoredState::Running {
        let _ = stop_container_sync(inner, id, 0);
    }
    let _ = inner.engine.remove_container(id);
    let _ = inner.engine.release_rootfs(id);
    inner
        .logs
        .write()
        .unwrap_or_else(|p| p.into_inner())
        .remove(id);
    inner.store.remove_container(id)?;
    // Only remove runtime-owned fallback logs; kubelet owns explicit log paths.
    let fallback_log = inner.config.root.join("logs").join(format!("{id}.log"));
    if std::path::Path::new(&record.log_path) == fallback_log {
        let _ = std::fs::remove_file(fallback_log);
    }
    inner.emit(id, ContainerEventType::ContainerDeletedEvent);
    Ok(())
}

pub(crate) fn container_stats_sync(inner: &Inner, id: &str) -> CrateResult<ContainerStats> {
    let record = inner.container_record(id)?;
    let raw = inner.engine.container_stats(id)?;
    Ok(build_container_stats(&record, &raw))
}

pub(crate) fn build_container_stats(
    record: &ContainerRecord,
    raw: &crate::engine::EngineStats,
) -> ContainerStats {
    let timestamp = now_ns();
    ContainerStats {
        attributes: Some(ContainerAttributes {
            id: record.id.clone(),
            metadata: Some(container_metadata(record)),
            labels: hashmap(&record.labels),
            annotations: hashmap(&record.annotations),
        }),
        cpu: Some(CpuUsage {
            timestamp,
            usage_core_nano_seconds: Some(UInt64Value {
                value: raw.cpu_total_ns,
            }),
            usage_nano_cores: Some(UInt64Value { value: 0 }),
            psi: None,
        }),
        memory: Some(MemoryUsage {
            timestamp,
            working_set_bytes: Some(UInt64Value {
                value: raw.mem_usage,
            }),
            available_bytes: Some(UInt64Value {
                value: raw.mem_limit.saturating_sub(raw.mem_usage),
            }),
            usage_bytes: Some(UInt64Value {
                value: raw.mem_usage + raw.mem_cache,
            }),
            rss_bytes: Some(UInt64Value { value: 0 }),
            page_faults: Some(UInt64Value { value: 0 }),
            major_page_faults: Some(UInt64Value { value: 0 }),
            psi: None,
        }),
        writable_layer: Some(FilesystemUsage {
            timestamp,
            fs_id: Some(FilesystemIdentifier {
                mountpoint: record.rootfs.clone(),
            }),
            used_bytes: Some(UInt64Value {
                value: raw.writable_size,
            }),
            inodes_used: Some(UInt64Value {
                value: raw.inodes_used,
            }),
        }),
        swap: Some(SwapUsage {
            timestamp,
            swap_available_bytes: Some(UInt64Value { value: 0 }),
            swap_usage_bytes: Some(UInt64Value {
                value: raw.mem_swap,
            }),
        }),
        io: Some(IoUsage {
            timestamp,
            psi: None,
        }),
    }
}

pub(crate) fn pod_sandbox_stats_sync(inner: &Inner, id: &str) -> CrateResult<PodSandboxStats> {
    let sandbox = inner.sandbox_record(id)?;
    let containers: Vec<ContainerStats> = inner
        .store
        .containers_in(id)
        .into_iter()
        .filter_map(|record| {
            inner
                .engine
                .container_stats(&record.id)
                .ok()
                .map(|raw| build_container_stats(&record, &raw))
        })
        .collect();
    let infra_stats = containers
        .iter()
        .find(|s| {
            s.attributes.as_ref().map(|a| a.id.as_str())
                == Some(sandbox.infra_container_id.as_str())
        })
        .cloned();
    Ok(PodSandboxStats {
        attributes: Some(PodSandboxAttributes {
            id: sandbox.id.clone(),
            metadata: Some(sandbox_metadata(&sandbox)),
            labels: hashmap(&sandbox.labels),
            annotations: hashmap(&sandbox.annotations),
        }),
        linux: Some(LinuxPodSandboxStats {
            cpu: infra_stats.as_ref().and_then(|s| s.cpu),
            memory: infra_stats.as_ref().and_then(|s| s.memory),
            network: Some(NetworkUsage {
                timestamp: now_ns(),
                default_interface: Some(NetworkInterfaceUsage {
                    name: "eth0".to_string(),
                    rx_bytes: Some(UInt64Value { value: 0 }),
                    rx_errors: Some(UInt64Value { value: 0 }),
                    tx_bytes: Some(UInt64Value { value: 0 }),
                    tx_errors: Some(UInt64Value { value: 0 }),
                }),
                interfaces: Vec::new(),
            }),
            process: Some(ProcessUsage {
                timestamp: now_ns(),
                process_count: Some(UInt64Value {
                    value: containers.len() as u64,
                }),
            }),
            containers,
            io: None,
        }),
        windows: None,
    })
}

pub(crate) fn metric_descriptors() -> Vec<MetricDescriptor> {
    [
        (
            "container_cpu_usage_seconds_total",
            "Cumulative CPU time consumed in seconds",
            MetricType::Counter,
        ),
        (
            "container_memory_working_set_bytes",
            "Current working set in bytes",
            MetricType::Gauge,
        ),
        (
            "container_fs_usage_bytes",
            "Writable layer bytes used",
            MetricType::Gauge,
        ),
        (
            "pod_cpu_usage_seconds_total",
            "Pod CPU time consumed in seconds",
            MetricType::Counter,
        ),
        (
            "pod_memory_working_set_bytes",
            "Pod working set in bytes",
            MetricType::Gauge,
        ),
    ]
    .into_iter()
    .map(|(name, help, _)| MetricDescriptor {
        name: name.to_string(),
        help: help.to_string(),
        label_keys: vec!["container_id".to_string()],
    })
    .collect()
}

pub(crate) fn stats_metrics(raw: &crate::engine::EngineStats) -> Vec<Metric> {
    vec![
        Metric {
            name: "container_cpu_usage_seconds_total".to_string(),
            timestamp: now_ns(),
            metric_type: MetricType::Counter as i32,
            label_values: Vec::new(),
            value: Some(UInt64Value {
                value: raw.cpu_total_ns / 1_000_000_000,
            }),
        },
        Metric {
            name: "container_memory_working_set_bytes".to_string(),
            timestamp: now_ns(),
            metric_type: MetricType::Gauge as i32,
            label_values: Vec::new(),
            value: Some(UInt64Value {
                value: raw.mem_usage,
            }),
        },
    ]
}

// Kept for handler unit tests: the synchronous core of `Version`.
#[cfg(test)]
pub(crate) fn version_response(runtime_name: &str) -> VersionResponse {
    VersionResponse {
        version: API_VERSION.to_string(),
        runtime_name: runtime_name.to_string(),
        runtime_version: env!("CARGO_PKG_VERSION").to_string(),
        runtime_api_version: API_VERSION.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sandbox_hostname_is_bounded_without_changing_explicit_names() {
        let config = PodSandboxConfig {
            metadata: Some(PodSandboxMetadata {
                name: format!("create-PodSandbox-for-container-{}", "a".repeat(36)),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(sandbox_hostname(&config).unwrap().len(), 64);
        let explicit = PodSandboxConfig {
            hostname: "custom-host".into(),
            ..config.clone()
        };
        assert_eq!(sandbox_hostname(&explicit).unwrap(), "custom-host");
        let invalid = PodSandboxConfig {
            hostname: "a".repeat(65),
            ..config
        };
        assert!(sandbox_hostname(&invalid).is_err());
    }

    #[test]
    fn exit_reason_distinguishes_oom_from_explicit_sigkill() {
        assert_eq!(exit_reason(137, true), "OOMKilled");
        assert_eq!(exit_reason(137, false), "Error");
        assert_eq!(exit_reason(0, true), "Completed");
    }

    #[test]
    fn status_preserves_requested_mounts() {
        let mut record = container("c", "s", StoredState::Created, None);
        let config = ContainerConfig {
            mounts: vec![Mount {
                host_path: "/host".into(),
                container_path: "/data".into(),
                propagation: MountPropagation::PropagationBidirectional as i32,
                readonly: true,
                ..Default::default()
            }],
            ..Default::default()
        };
        record.config_bytes = config.encode_to_vec();
        let restored: ContainerRecord =
            serde_json::from_str(&serde_json::to_string(&record).unwrap()).unwrap();
        assert_eq!(container_status(&restored).mounts, config.mounts);
    }

    #[test]
    fn signal_names_map_to_numbers() {
        assert_eq!(signal_number("SIGTERM"), 15);
        assert_eq!(signal_number("SIGKILL"), 9);
        assert_eq!(signal_number("9"), 9);
        assert_eq!(signal_number(""), 15);
    }

    #[test]
    fn version_reports_cri_v1() {
        let version = version_response("oci-cri");
        assert_eq!(version.runtime_name, "oci-cri");
        assert_eq!(version.runtime_api_version, "v1");
    }

    fn sandbox(id: &str, ready: bool, label: Option<(&str, &str)>) -> SandboxRecord {
        let mut labels = BTreeMap::new();
        if let Some((k, v)) = label {
            labels.insert(k.into(), v.into());
        }
        SandboxRecord {
            id: id.into(),
            metadata: StoredMetadata {
                name: "pod".into(),
                uid: "uid".into(),
                namespace: "ns".into(),
                attempt: 1,
            },
            labels,
            annotations: BTreeMap::new(),
            created_at_ns: 1,
            state: if ready {
                SandboxState::Ready
            } else {
                SandboxState::NotReady
            },
            infra_container_id: "infra".into(),
            ip: String::new(),
            extra_ips: Vec::new(),
            host_network: false,
            linux_json: String::new(),
            log_directory: String::new(),
        }
    }

    fn container(
        id: &str,
        sandbox_id: &str,
        state: StoredState,
        label: Option<(&str, &str)>,
    ) -> ContainerRecord {
        let mut labels = BTreeMap::new();
        if let Some((k, v)) = label {
            labels.insert(k.into(), v.into());
        }
        ContainerRecord {
            id: id.into(),
            sandbox_id: sandbox_id.into(),
            metadata: StoredMetadata {
                name: "app".into(),
                uid: String::new(),
                namespace: String::new(),
                attempt: 1,
            },
            image: "img".into(),
            image_ref: String::new(),
            labels,
            annotations: BTreeMap::new(),
            log_path: String::new(),
            tty: false,
            stdin: false,
            stdin_once: false,
            state,
            created_at_ns: 1,
            started_at_ns: 0,
            finished_at_ns: 0,
            exit_code: -1,
            reason: String::new(),
            message: String::new(),
            spec_json: String::new(),
            config_bytes: Vec::new(),
            rootfs: String::new(),
            stop_signal: String::new(),
        }
    }

    #[test]
    fn sandbox_filters_by_id_state_and_labels() {
        let ready = sandbox("sb-ready", true, Some(("tier", "front")));
        let stopped = sandbox("sb-stop", false, Some(("tier", "back")));
        assert!(sandbox_matches(&ready, None));
        assert!(sandbox_matches(
            &ready,
            Some(&PodSandboxFilter {
                id: "sb-ready".into(),
                ..PodSandboxFilter::default()
            })
        ));
        assert!(!sandbox_matches(
            &ready,
            Some(&PodSandboxFilter {
                id: "other".into(),
                ..PodSandboxFilter::default()
            })
        ));
        assert!(sandbox_matches(
            &ready,
            Some(&PodSandboxFilter {
                state: Some(PodSandboxStateValue {
                    state: PodSandboxState::SandboxReady as i32,
                }),
                ..PodSandboxFilter::default()
            })
        ));
        assert!(!sandbox_matches(
            &stopped,
            Some(&PodSandboxFilter {
                state: Some(PodSandboxStateValue {
                    state: PodSandboxState::SandboxReady as i32,
                }),
                ..PodSandboxFilter::default()
            })
        ));
        assert!(sandbox_matches(
            &ready,
            Some(&PodSandboxFilter {
                label_selector: HashMap::from([("tier".into(), "front".into())]),
                ..PodSandboxFilter::default()
            })
        ));
        assert!(!sandbox_matches(
            &ready,
            Some(&PodSandboxFilter {
                label_selector: HashMap::from([("tier".into(), "back".into())]),
                ..PodSandboxFilter::default()
            })
        ));
    }

    #[test]
    fn container_filters_by_sandbox_state_and_labels() {
        let running = container("c1", "sb1", StoredState::Running, Some(("app", "web")));
        let created = container("c2", "sb2", StoredState::Created, None);
        assert!(container_matches(
            &running,
            Some(&ContainerFilter {
                pod_sandbox_id: "sb1".into(),
                ..ContainerFilter::default()
            })
        ));
        assert!(!container_matches(
            &running,
            Some(&ContainerFilter {
                pod_sandbox_id: "sb2".into(),
                ..ContainerFilter::default()
            })
        ));
        assert!(container_matches(
            &running,
            Some(&ContainerFilter {
                state: Some(ContainerStateValue {
                    state: ContainerState::ContainerRunning as i32,
                }),
                ..ContainerFilter::default()
            })
        ));
        assert!(!container_matches(
            &created,
            Some(&ContainerFilter {
                state: Some(ContainerStateValue {
                    state: ContainerState::ContainerRunning as i32,
                }),
                ..ContainerFilter::default()
            })
        ));
        assert!(container_matches(
            &running,
            Some(&ContainerFilter {
                label_selector: HashMap::from([("app".into(), "web".into())]),
                ..ContainerFilter::default()
            })
        ));
        assert!(stats_container_matches(
            &running,
            Some(&ContainerStatsFilter {
                id: "c1".into(),
                pod_sandbox_id: "sb1".into(),
                label_selector: HashMap::from([("app".into(), "web".into())]),
            })
        ));
        assert!(!stats_container_matches(
            &running,
            Some(&ContainerStatsFilter {
                id: "c1".into(),
                pod_sandbox_id: "other".into(),
                ..ContainerStatsFilter::default()
            })
        ));
    }

    #[test]
    fn metric_descriptors_are_stable() {
        let descriptors = metric_descriptors();
        assert!(
            descriptors
                .iter()
                .any(|d| d.name == "container_cpu_usage_seconds_total")
        );
        assert!(descriptors.iter().any(|d| d.name.contains("memory")));
    }
}

#[cfg(all(test, unix))]
mod network_teardown_regressions {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[tokio::test]
    async fn teardown_targets_live_sandbox_and_skips_host_network() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        let conf = dir.path().join("conf");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::create_dir_all(&conf).unwrap();
        let plugin = bin.join("probe");
        std::fs::write(
            &plugin,
            r#"#!/bin/sh
cat >/dev/null
printf '%s %s\n' "$CNI_COMMAND" "$CNI_NETNS" >> "$(dirname "$0")/calls"
if [ "$CNI_COMMAND" = DEL ]; then
    pid=${CNI_NETNS#/proc/}
    pid=${pid%%/*}
    kill -0 "$pid" && echo alive >> "$(dirname "$0")/calls"
fi
echo '{"cniVersion":"1.0.0","ips":[{"address":"10.0.0.7/24"}]}'
"#,
        )
        .unwrap();
        std::fs::set_permissions(&plugin, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(
            conf.join("probe.conflist"),
            r#"{"cniVersion":"1.0.0","name":"probe","plugins":[{"type":"probe"}]}"#,
        )
        .unwrap();
        let root = dir.path().join("root");
        let engine = Arc::new(crate::test_support::FakeEngine::new(root.clone()));
        let service = CriService::open(
            Config {
                root,
                state_root: dir.path().join("state"),
                cni_bin_dir: Some(bin.clone()),
                cni_conf_dir: Some(conf),
                ..Config::default()
            },
            engine.clone(),
        )
        .unwrap();
        for host_network in [false, true] {
            let config = PodSandboxConfig {
                linux: Some(LinuxPodSandboxConfig {
                    security_context: Some(LinuxSandboxSecurityContext {
                        namespace_options: Some(NamespaceOption {
                            network: if host_network {
                                NamespaceMode::Node as i32
                            } else {
                                NamespaceMode::Pod as i32
                            },
                            ..Default::default()
                        }),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            };
            let sandbox = run_pod_sandbox_sync(
                &service.inner,
                RunPodSandboxRequest {
                    config: Some(config),
                    ..Default::default()
                },
            )
            .unwrap();
            let record = service.inner.sandbox_record(&sandbox).unwrap();
            let pid = engine
                .container_state(&record.infra_container_id)
                .unwrap()
                .pid;
            stop_pod_sandbox_sync(&service.inner, &sandbox).unwrap();
            let calls = std::fs::read_to_string(bin.join("calls")).unwrap();
            if !host_network {
                assert!(
                    calls.contains(&format!("DEL /proc/{pid}/ns/net\nalive\n")),
                    "{calls}"
                );
            } else {
                assert!(
                    !calls.contains(&format!("/proc/{pid}/ns/net")),
                    "host network was passed to CNI: {calls}"
                );
            }
            assert!(!calls.contains("/proc/self/ns/net"));
        }
    }
}

#[cfg(test)]
mod dns_regressions {
    use super::*;

    #[tokio::test]
    async fn workloads_mount_their_sandbox_dns_and_removal_cleans_it() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        let service = CriService::open(
            Config {
                root: root.clone(),
                state_root: dir.path().join("state"),
                ..Default::default()
            },
            Arc::new(crate::test_support::FakeEngine::new(root)),
        )
        .unwrap();
        let image = service.inner.ensure_image("fake/app:latest").unwrap();
        for server in ["10.43.0.10", "10.44.0.10"] {
            let pod = run_pod_sandbox_sync(
                &service.inner,
                RunPodSandboxRequest {
                    config: Some(PodSandboxConfig {
                        dns_config: Some(DnsConfig {
                            servers: vec![server.into()],
                            searches: vec!["default.svc.cluster.local".into()],
                            options: vec!["ndots:5".into()],
                        }),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
            )
            .unwrap();
            let id = create_container_sync(
                &service.inner,
                CreateContainerRequest {
                    pod_sandbox_id: pod.clone(),
                    config: Some(ContainerConfig {
                        image: Some(ImageSpec {
                            image: image.id.clone(),
                            ..Default::default()
                        }),
                        command: vec!["sleep".into(), "30".into()],
                        ..Default::default()
                    }),
                    ..Default::default()
                },
            )
            .unwrap();
            let record = service.inner.container_record(&id).unwrap();
            let spec: serde_json::Value = serde_json::from_str(&record.spec_json).unwrap();
            let mounts: Vec<_> = spec["mounts"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|mount| mount["destination"] == "/etc/resolv.conf")
                .collect();
            assert_eq!(mounts.len(), 1);
            let source = mounts[0]["source"].as_str().unwrap();
            assert_ne!(source, "/etc/resolv.conf");
            assert_eq!(
                std::fs::read_to_string(source).unwrap(),
                format!("nameserver {server}\nsearch default.svc.cluster.local\noptions ndots:5\n")
            );
            RuntimeService::remove_pod_sandbox(
                &service,
                Request::new(RemovePodSandboxRequest {
                    pod_sandbox_id: pod,
                }),
            )
            .await
            .unwrap();
            assert!(!std::path::Path::new(source).exists());
        }
    }
}
