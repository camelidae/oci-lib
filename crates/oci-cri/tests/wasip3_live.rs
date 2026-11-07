//! Live, opt-in test: requires a privileged Linux CRI daemon, the packaged runner,
//! and the single/indexed/mixed tags published by Diorama's wasip3/registry.py.
use oci_cri::api::{
    image_service_client::ImageServiceClient, runtime_service_client::RuntimeServiceClient, *,
};
use tonic::transport::{Endpoint, Uri};

#[tokio::test]
#[ignore = "requires a live Linux CRI daemon and fixture registry"]
async fn wasm_and_native_images() {
    let socket = std::env::var("CRI_SOCKET").unwrap_or("/run/oci-cri.sock".into());
    let repo = std::env::var("P3_REPOSITORY").expect("P3_REPOSITORY");
    let channel = Endpoint::from_static("http://localhost")
        .connect_with_connector(tower::service_fn(move |_: Uri| {
            let socket = socket.clone();
            async move {
                Ok::<_, std::io::Error>(hyper_util::rt::TokioIo::new(
                    tokio::net::UnixStream::connect(socket).await?,
                ))
            }
        }))
        .await
        .unwrap();
    let mut images = ImageServiceClient::new(channel.clone());
    let mut runtime = RuntimeServiceClient::new(channel);
    // Alternating handlers on the same mixed tag detects stale platform caches.
    for (n, (tag, handler, mode)) in [
        ("single", "wasmtime", "hello"),
        ("indexed", "wasmtime", "hello"),
        ("mixed", "wasmtime", "hello"),
        ("mixed", "", "hello"),
        ("mixed", "wasmtime", "hello"),
        ("single", "wasmtime", "exit"),
    ]
    .into_iter()
    .enumerate()
    {
        let image = ImageSpec {
            image: format!("{repo}:{tag}"),
            runtime_handler: handler.into(),
            ..Default::default()
        };
        let id = images
            .pull_image(PullImageRequest {
                image: Some(image.clone()),
                ..Default::default()
            })
            .await
            .unwrap()
            .into_inner()
            .image_ref;
        let found = images
            .image_status(ImageStatusRequest {
                image: Some(image),
                verbose: true,
            })
            .await
            .unwrap()
            .into_inner();
        assert_eq!(found.image.unwrap().id, id);
        let name = format!("p3-live-{}-{n}", std::process::id());
        let logs = format!(
            "{}/{name}",
            std::env::var("CRI_LOG_ROOT").unwrap_or("/tmp".into())
        );
        let host_logs = format!(
            "{}/{name}",
            std::env::var("CRI_HOST_LOG_ROOT")
                .unwrap_or_else(|_| std::env::var("CRI_LOG_ROOT").unwrap_or("/tmp".into()))
        );
        std::fs::create_dir_all(&host_logs).unwrap();
        let sandbox_config = PodSandboxConfig {
            metadata: Some(PodSandboxMetadata {
                name: name.clone(),
                namespace: "default".into(),
                uid: name.clone(),
                ..Default::default()
            }),
            log_directory: logs.clone(),
            linux: Some(LinuxPodSandboxConfig {
                security_context: Some(LinuxSandboxSecurityContext {
                    namespace_options: Some(NamespaceOption {
                        network: NamespaceMode::Node as i32,
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let sandbox = runtime
            .run_pod_sandbox(RunPodSandboxRequest {
                config: Some(sandbox_config.clone()),
                runtime_handler: handler.into(),
            })
            .await
            .unwrap()
            .into_inner()
            .pod_sandbox_id;
        let native = handler.is_empty();
        let config = ContainerConfig {
            metadata: Some(ContainerMetadata {
                name,
                ..Default::default()
            }),
            image: Some(ImageSpec {
                image: id,
                runtime_handler: handler.into(),
                ..Default::default()
            }),
            command: if native {
                vec!["/bin/sh".into(), "-c".into(), "echo native-ok".into()]
            } else {
                vec!["/probe.wasm".into()]
            },
            args: if native {
                vec![]
            } else {
                vec![mode.into(), "from-cri".into()]
            },
            envs: vec![KeyValue {
                key: "P3_TEST".into(),
                value: "from-env".into(),
            }],
            log_path: "container.log".into(),
            linux: Some(LinuxContainerConfig {
                resources: Some(LinuxContainerResources {
                    memory_limit_in_bytes: 134217728,
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let container = runtime
            .create_container(CreateContainerRequest {
                pod_sandbox_id: sandbox.clone(),
                config: Some(config),
                sandbox_config: Some(sandbox_config),
            })
            .await
            .unwrap()
            .into_inner()
            .container_id;
        runtime
            .start_container(StartContainerRequest {
                container_id: container.clone(),
            })
            .await
            .unwrap();
        let status = tokio::time::timeout(std::time::Duration::from_secs(90), async {
            loop {
                let s = runtime
                    .container_status(ContainerStatusRequest {
                        container_id: container.clone(),
                        verbose: false,
                    })
                    .await
                    .unwrap()
                    .into_inner()
                    .status
                    .unwrap();
                if s.state == ContainerState::ContainerExited as i32 {
                    break s;
                }
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
        })
        .await
        .unwrap();
        let output = std::fs::read_to_string(format!("{host_logs}/container.log")).unwrap();
        assert_eq!(
            status.exit_code,
            if mode == "exit" { 7 } else { 0 },
            "{output}"
        );
        assert!(
            mode == "exit"
                || output.contains(if native {
                    "native-ok"
                } else {
                    "hello from WASI 0.3"
                }),
            "{output}"
        );
        runtime
            .remove_container(RemoveContainerRequest {
                container_id: container,
            })
            .await
            .unwrap();
        runtime
            .stop_pod_sandbox(StopPodSandboxRequest {
                pod_sandbox_id: sandbox.clone(),
            })
            .await
            .unwrap();
        runtime
            .remove_pod_sandbox(RemovePodSandboxRequest {
                pod_sandbox_id: sandbox,
            })
            .await
            .unwrap();
        println!("passed {tag} handler={handler:?}: {output}");
    }
}
