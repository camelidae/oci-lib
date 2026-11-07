// SPDX-License-Identifier: Apache-2.0

//! Virtio-vsock agent. The macOS guest runs this binary from `/init`.
//!
//! Listens on [`crate::proto::VSOCK_PORT`], writes [`GUEST_BANNER`] on each
//! accept, then proxies bytes to the in-guest CRI unix socket.

use std::fs::File;
use std::io::{self, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;

use crate::config::Config;
use crate::engine::Engine;
use crate::error::{Error, ErrorCode};
use crate::proto::{GUEST_BANNER, VSOCK_PORT};
use crate::server;

/// Guest-side agent entry point (`oci-cri --engine-serve`).
pub fn engine_serve() -> Result<i32, Error> {
    // Host exports Config::root as virtiofs tag `root` and Config::state_root
    // as tag `runroot`. Init mounts those at /mnt/root and /mnt/runroot (with
    // an ext image overlay on root for containers/storage).
    let root = PathBuf::from("/mnt/root");
    let state_root = PathBuf::from("/mnt/runroot");
    let socket = PathBuf::from("/run/oci-cri/oci-cri.sock");
    std::fs::create_dir_all(&root).map_err(io_err)?;
    std::fs::create_dir_all(&state_root).map_err(io_err)?;
    std::fs::create_dir_all("/run/oci-cri").map_err(io_err)?;

    let config = Config {
        runtime_config_dir: Some(PathBuf::from("/etc/oci-cri/config.d")),
        runtimes: Default::default(),
        runtime_name: crate::config::RUNTIME_NAME.to_string(),
        socket: socket.clone(),
        root: root.clone(),
        state_root: state_root.clone(),
        sandbox_image: "registry.k8s.io/pause:3.10.1".to_string(),
        // CNI on the macOS guest is out of scope for the MVP; sandboxes share
        // the guest network namespace.
        cni_bin_dir: None,
        cni_conf_dir: None,
        cni_plugin_dirs: Vec::new(),
        streaming_address: "127.0.0.1".to_string(),
        streaming_port: 0,
        pod_cidr: String::new(),
        insecure_registries: false,
    };
    config.validate()?;

    crate::linux::open()?;
    let engine: Arc<dyn Engine> = Arc::new(crate::linux::LinuxEngine::new(root, state_root));

    let listener = vsock_listen(VSOCK_PORT)
        .map_err(|err| Error::new(ErrorCode::Prerequisite, format!("vsock listen: {err}"), ""))?;

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|err| Error::new(ErrorCode::Internal, err.to_string(), ""))?;

    let serve_config = config.clone();
    let serve_engine = Arc::clone(&engine);
    let cri = thread::Builder::new()
        .name("oci-cri-grpc".into())
        .spawn(move || {
            let result = runtime.block_on(server::serve_with_engine(serve_config, serve_engine));
            if let Err(err) = result {
                eprintln!("oci-cri: guest CRI server: {err}");
            }
        })
        .map_err(|err| Error::new(ErrorCode::Internal, format!("guest CRI thread: {err}"), ""))?;

    wait_for_socket(&socket)?;

    let result = accept_loop(listener, &socket);
    let _ = crate::linux::shutdown();
    let _ = cri.join();
    result
}

fn accept_loop(listener: OwnedFd, socket: &PathBuf) -> Result<i32, Error> {
    loop {
        let mut vsock = vsock_accept(&listener)
            .map_err(|err| Error::new(ErrorCode::Internal, format!("vsock accept: {err}"), ""))?;
        vsock
            .write_all(GUEST_BANNER)
            .map_err(|err| Error::new(ErrorCode::Internal, format!("vsock banner: {err}"), ""))?;
        let mut unix = match std::os::unix::net::UnixStream::connect(socket) {
            Ok(stream) => stream,
            Err(err) => {
                eprintln!("oci-cri: dialing guest CRI socket: {err}");
                continue;
            }
        };
        let mut vsock_peer = vsock
            .try_clone()
            .map_err(|err| Error::new(ErrorCode::Internal, format!("vsock clone: {err}"), ""))?;
        let mut unix_peer = unix
            .try_clone()
            .map_err(|err| Error::new(ErrorCode::Internal, format!("unix clone: {err}"), ""))?;
        thread::spawn(move || {
            let _ = io::copy(&mut vsock, &mut unix);
        });
        thread::spawn(move || {
            let _ = io::copy(&mut unix_peer, &mut vsock_peer);
        });
    }
}

fn wait_for_socket(socket: &Path) -> Result<(), Error> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while std::time::Instant::now() < deadline {
        if socket.exists() {
            return Ok(());
        }
        thread::sleep(std::time::Duration::from_millis(50));
    }
    Err(Error::new(
        ErrorCode::Prerequisite,
        format!("guest CRI socket {} did not appear", socket.display()),
        "",
    ))
}

fn vsock_listen(port: u32) -> io::Result<OwnedFd> {
    let fd = unsafe { libc::socket(libc::AF_VSOCK, libc::SOCK_STREAM, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    let addr = libc::sockaddr_vm {
        svm_family: libc::AF_VSOCK as libc::sa_family_t,
        svm_reserved1: 0,
        svm_port: port,
        svm_cid: libc::VMADDR_CID_ANY,
        svm_zero: [0; 4],
    };
    let rc = unsafe {
        libc::bind(
            fd.as_raw_fd(),
            &addr as *const libc::sockaddr_vm as *const libc::sockaddr,
            std::mem::size_of::<libc::sockaddr_vm>() as libc::socklen_t,
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    if unsafe { libc::listen(fd.as_raw_fd(), 128) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(fd)
}

fn vsock_accept(listener: &OwnedFd) -> io::Result<File> {
    let mut addr = unsafe { std::mem::zeroed::<libc::sockaddr_vm>() };
    let mut len = std::mem::size_of::<libc::sockaddr_vm>() as libc::socklen_t;
    let fd = unsafe {
        libc::accept(
            listener.as_raw_fd(),
            &mut addr as *mut libc::sockaddr_vm as *mut libc::sockaddr,
            &mut len,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { File::from(OwnedFd::from_raw_fd(fd)) })
}

fn io_err(err: io::Error) -> Error {
    Error::new(ErrorCode::Internal, err.to_string(), "")
}
