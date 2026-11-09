// SPDX-License-Identifier: Apache-2.0

//! End-to-end CRI lifecycle over a real unix-socket gRPC server, using the
//! fake engine (child processes instead of libcontainer). Exercises the wire
//! protocol kubelet and crictl speak: pull, sandbox, create/start/status,
//! logs, exec, stats, stop/remove.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use oci_cri::Config;
use oci_cri::api::image_service_client::ImageServiceClient;
use oci_cri::api::runtime_service_client::RuntimeServiceClient;
use oci_cri::api::*;
use oci_cri::engine::Engine;
use oci_cri::test_support::FakeEngine;
use tonic::transport::{Channel, Endpoint, Uri};
use tower::service_fn;

async fn client(socket: PathBuf) -> (RuntimeServiceClient<Channel>, ImageServiceClient<Channel>) {
    let channel = Endpoint::from_static("http://localhost")
        .connect_with_connector(service_fn(move |_: Uri| {
            let socket = socket.clone();
            async move {
                let stream = tokio::net::UnixStream::connect(&socket).await?;
                Ok::<_, std::io::Error>(hyper_util::rt::TokioIo::new(stream))
            }
        }))
        .await
        .expect("connect to CRI socket");
    (
        RuntimeServiceClient::new(channel.clone()),
        ImageServiceClient::new(channel),
    )
}

async fn wait_for_socket(path: &Path) {
    for _ in 0..100 {
        if path.exists() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("socket {} never appeared", path.display());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cri_lifecycle_end_to_end() {
    let dir = tempfile::tempdir().expect("tempdir");
    let socket = dir.path().join("oci-cri.sock");
    let root = dir.path().join("root");
    let config = Config {
        socket: socket.clone(),
        root: root.clone(),
        state_root: dir.path().join("state"),
        sandbox_image: "fake/pause:latest".to_string(),
        streaming_address: "127.0.0.1".to_string(),
        streaming_port: 0,
        ..Config::default()
    };
    let engine: Arc<dyn Engine> = Arc::new(FakeEngine::new(root));
    let server_config = config.clone();
    let server_engine = Arc::clone(&engine);
    let server = tokio::spawn(async move {
        oci_cri::server::serve_with_engine(server_config, server_engine)
            .await
            .expect("serve");
    });
    wait_for_socket(&socket).await;
    let (mut runtime, mut images) = client(socket).await;

    let version = runtime
        .version(VersionRequest {
            version: "v1".to_string(),
        })
        .await
        .expect("version")
        .into_inner();
    assert_eq!(version.runtime_name, "oci-cri");
    assert_eq!(version.runtime_api_version, "v1");

    let pulled = images
        .pull_image(PullImageRequest {
            image: Some(ImageSpec {
                image: "fake/app:latest".to_string(),
                ..ImageSpec::default()
            }),
            auth: None,
            sandbox_config: None,
        })
        .await
        .expect("pull")
        .into_inner();
    assert!(
        pulled.image_ref.contains("fake/app"),
        "{}",
        pulled.image_ref
    );

    let listed = images
        .list_images(ListImagesRequest { filter: None })
        .await
        .expect("list images")
        .into_inner();
    assert_eq!(listed.images.len(), 1);

    // The daemon pulls the sandbox image itself on first use.
    let sandbox = runtime
        .run_pod_sandbox(RunPodSandboxRequest {
            config: Some(PodSandboxConfig {
                metadata: Some(PodSandboxMetadata {
                    name: "pod-a".to_string(),
                    uid: "uid-a".to_string(),
                    namespace: "default".to_string(),
                    attempt: 1,
                }),
                hostname: "pod-a".to_string(),
                log_directory: dir.path().join("logs").to_string_lossy().into_owned(),
                labels: HashMap::from([("app".to_string(), "web".to_string())]),
                linux: Some(LinuxPodSandboxConfig {
                    security_context: Some(LinuxSandboxSecurityContext {
                        namespace_options: Some(NamespaceOption {
                            network: NamespaceMode::Pod as i32,
                            ..NamespaceOption::default()
                        }),
                        ..LinuxSandboxSecurityContext::default()
                    }),
                    ..LinuxPodSandboxConfig::default()
                }),
                ..PodSandboxConfig::default()
            }),
            runtime_handler: String::new(),
        })
        .await
        .expect("run sandbox")
        .into_inner();
    let sandbox_id = sandbox.pod_sandbox_id;
    assert_eq!(sandbox_id.len(), 64);

    let created = runtime
        .create_container(CreateContainerRequest {
            pod_sandbox_id: sandbox_id.clone(),
            config: Some(ContainerConfig {
                metadata: Some(ContainerMetadata {
                    name: "app".to_string(),
                    attempt: 1,
                }),
                image: Some(ImageSpec {
                    image: "fake/app:latest".to_string(),
                    ..ImageSpec::default()
                }),
                command: vec!["/bin/sh".to_string()],
                args: vec![
                    "-c".to_string(),
                    "echo hello-from-container; sleep 30".to_string(),
                ],
                log_path: "app/0.log".to_string(),
                ..ContainerConfig::default()
            }),
            sandbox_config: None,
        })
        .await
        .expect("create")
        .into_inner();
    let container_id = created.container_id;

    runtime
        .start_container(StartContainerRequest {
            container_id: container_id.clone(),
        })
        .await
        .expect("start");

    // ExecSync against the running container.
    let exec = runtime
        .exec_sync(ExecSyncRequest {
            container_id: container_id.clone(),
            cmd: vec!["/bin/echo".to_string(), "exec-ok".to_string()],
            timeout: 10,
        })
        .await
        .expect("exec sync")
        .into_inner();
    assert_eq!(exec.exit_code, 0);
    assert_eq!(String::from_utf8_lossy(&exec.stdout).trim(), "exec-ok");

    // Log output lands in CRI format.
    let mut log_text = String::new();
    for _ in 0..50 {
        let status = runtime
            .container_status(ContainerStatusRequest {
                container_id: container_id.clone(),
                verbose: false,
            })
            .await
            .expect("status")
            .into_inner();
        assert_eq!(
            status.status.as_ref().unwrap().state,
            ContainerState::ContainerRunning as i32
        );
        let log_path = dir.path().join("logs/app/0.log");
        if let Ok(text) = std::fs::read_to_string(&log_path) {
            if text.contains("hello-from-container") {
                log_text = text;
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        log_text
            .lines()
            .any(|l| l.ends_with("stdout F hello-from-container")),
        "log: {log_text}"
    );

    let stats = runtime
        .container_stats(ContainerStatsRequest {
            container_id: container_id.clone(),
        })
        .await
        .expect("stats")
        .into_inner();
    assert!(stats.stats.is_some());

    let containers = runtime
        .list_containers(ListContainersRequest { filter: None })
        .await
        .expect("list containers")
        .into_inner();
    assert_eq!(containers.containers.len(), 1);

    runtime
        .stop_container(StopContainerRequest {
            container_id: container_id.clone(),
            timeout: 1,
        })
        .await
        .expect("stop");
    let status = runtime
        .container_status(ContainerStatusRequest {
            container_id: container_id.clone(),
            verbose: false,
        })
        .await
        .expect("status after stop")
        .into_inner();
    assert_eq!(
        status.status.as_ref().unwrap().state,
        ContainerState::ContainerExited as i32
    );

    runtime
        .remove_container(RemoveContainerRequest {
            container_id: container_id.clone(),
        })
        .await
        .expect("remove container");
    runtime
        .stop_pod_sandbox(StopPodSandboxRequest {
            pod_sandbox_id: sandbox_id.clone(),
        })
        .await
        .expect("stop sandbox");
    runtime
        .remove_pod_sandbox(RemovePodSandboxRequest {
            pod_sandbox_id: sandbox_id.clone(),
        })
        .await
        .expect("remove sandbox");
    let sandboxes = runtime
        .list_pod_sandbox(ListPodSandboxRequest { filter: None })
        .await
        .expect("list sandboxes")
        .into_inner();
    assert!(sandboxes.items.is_empty());

    server.abort();
}

/// Run against a live guest proxy (`oci-cri serve` built with `--features vm`):
/// `CRI_SOCKET=/path/to/oci-cri.sock cargo test -p oci-cri --test cri_lifecycle guest_cni_assigns_pod_sandbox_ip -- --ignored --nocapture`
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires a live CRI daemon with guest CNI enabled (set CRI_SOCKET)"]
async fn guest_cni_assigns_pod_sandbox_ip() {
    let socket = PathBuf::from(
        std::env::var("CRI_SOCKET").unwrap_or_else(|_| "/run/oci-cri/oci-cri.sock".to_string()),
    );
    wait_for_socket(&socket).await;
    let (mut runtime, _images) = client(socket).await;

    let sandbox = runtime
        .run_pod_sandbox(RunPodSandboxRequest {
            config: Some(PodSandboxConfig {
                metadata: Some(PodSandboxMetadata {
                    name: "cni-smoke".to_string(),
                    uid: "cni-smoke-uid".to_string(),
                    namespace: "default".to_string(),
                    attempt: 1,
                }),
                hostname: "cni-smoke".to_string(),
                linux: Some(LinuxPodSandboxConfig {
                    security_context: Some(LinuxSandboxSecurityContext {
                        namespace_options: Some(NamespaceOption {
                            network: NamespaceMode::Pod as i32,
                            ..NamespaceOption::default()
                        }),
                        ..LinuxSandboxSecurityContext::default()
                    }),
                    ..LinuxPodSandboxConfig::default()
                }),
                ..PodSandboxConfig::default()
            }),
            runtime_handler: String::new(),
        })
        .await
        .expect("run non-host-network pod sandbox")
        .into_inner();
    let sandbox_id = sandbox.pod_sandbox_id;

    let status = runtime
        .pod_sandbox_status(PodSandboxStatusRequest {
            pod_sandbox_id: sandbox_id.clone(),
            verbose: false,
        })
        .await
        .expect("pod sandbox status")
        .into_inner();
    let ip: std::net::Ipv4Addr = status
        .status
        .as_ref()
        .and_then(|s| s.network.as_ref())
        .map(|n| n.ip.as_str())
        .unwrap_or_default()
        .parse()
        .expect("sandbox should receive an IPv4 address from guest CNI");
    println!("sandbox {sandbox_id} assigned CNI IP: {ip}");
    assert!(
        !ip.is_unspecified() && !ip.is_loopback(),
        "unexpected sandbox IP {ip}"
    );

    runtime
        .stop_pod_sandbox(StopPodSandboxRequest {
            pod_sandbox_id: sandbox_id.clone(),
        })
        .await
        .expect("stop sandbox");
    runtime
        .remove_pod_sandbox(RemovePodSandboxRequest {
            pod_sandbox_id: sandbox_id,
        })
        .await
        .expect("remove sandbox");
}
