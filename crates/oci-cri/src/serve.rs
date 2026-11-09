// SPDX-License-Identifier: Apache-2.0

//! Virtio-vsock agent. The macOS guest runs this binary from `/init`.
//!
//! Listens on [`crate::proto::VSOCK_PORT`], writes [`GUEST_BANNER`] on each
//! accept, then proxies bytes to the in-guest CRI unix socket.

#[cfg(target_os = "linux")]
use std::ffi::CStr;
#[cfg(target_os = "linux")]
use std::fs::File;
use std::io;
#[cfg(target_os = "linux")]
use std::io::Write;
use std::net::Ipv4Addr;
#[cfg(target_os = "linux")]
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
#[cfg(target_os = "linux")]
use std::path::{Path, PathBuf};
#[cfg(target_os = "linux")]
use std::sync::Arc;
#[cfg(target_os = "linux")]
use std::thread;

#[cfg(target_os = "linux")]
use crate::config::Config;
#[cfg(target_os = "linux")]
use crate::engine::Engine;
#[cfg(target_os = "linux")]
use crate::error::{Error, ErrorCode};
#[cfg(target_os = "linux")]
use crate::proto::{GUEST_BANNER, VSOCK_PORT};
#[cfg(target_os = "linux")]
use crate::server;

const NLMSG_ERROR: u16 = 2;
const RTM_NEWLINK: u16 = 16;
const RTM_DELLINK: u16 = 17;
const NLM_F_REQUEST: u16 = 0x0001;
const NLM_F_ACK: u16 = 0x0004;
const NLM_F_EXCL: u16 = 0x0200;
const NLM_F_CREATE: u16 = 0x0400;

const IFLA_IFNAME: u16 = 3;
const IFLA_LINKINFO: u16 = 18;
const IFLA_INFO_KIND: u16 = 1;
const IFLA_INFO_DATA: u16 = 2;
const VETH_INFO_PEER: u16 = 1;

const NLMSG_HDRLEN: usize = 16;
const IFINFOMSG_LEN: usize = 16;

/// Guest-side agent entry point (`oci-cri --engine-serve`).
#[cfg(target_os = "linux")]
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

    let mut config = Config {
        runtime_config_dir: Some(PathBuf::from("/etc/oci-cri/config.d")),
        runtimes: Default::default(),
        runtime_name: crate::config::RUNTIME_NAME.to_string(),
        socket: socket.clone(),
        root: root.clone(),
        state_root: state_root.clone(),
        host_shares: Vec::new(),
        sandbox_image: "registry.k8s.io/pause:3.10.1".to_string(),
        cni_bin_dir: None,
        cni_conf_dir: None,
        cni_plugin_dirs: Vec::new(),
        streaming_address: "127.0.0.1".to_string(),
        streaming_port: 0,
        pod_cidr: String::new(),
        insecure_registries: false,
    };
    configure_guest_network(&mut config)?;
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

#[cfg(target_os = "linux")]
fn configure_guest_network(config: &mut Config) -> Result<(), Error> {
    let Some((interface, address, prefix)) = guest_uplink() else {
        eprintln!("oci-cri: no guest NAT IPv4 uplink found; CNI remains disabled");
        return Ok(());
    };
    // Streaming URLs still need the NAT-facing address even when CNI is off.
    config.streaming_address = address.to_string();

    // Built-in CONFIG_VETH=y does not expose /sys/module/veth, so functionally
    // probe creating and deleting a veth pair and bridge via rtnetlink.
    if std::env::var_os("ROK_DISABLE_GUEST_CNI").is_some() {
        eprintln!("oci-cri: guest CNI disabled by ROK_DISABLE_GUEST_CNI; streaming on {address}");
        return Ok(());
    }
    if let Err(reason) = probe_guest_cni() {
        eprintln!(
            "oci-cri: guest CNI probe failed ({reason}); CNI disabled (sandboxes share guest netns); streaming on {address}"
        );
        return Ok(());
    }

    let Some(pod_network) = crate::network::guest_pod_network(address, prefix) else {
        eprintln!(
            "oci-cri: guest NAT subnet {address}/{prefix} is too small for a pod range; CNI remains disabled"
        );
        return Ok(());
    };

    let config_dir = PathBuf::from("/etc/cni/net.d");
    std::fs::create_dir_all(&config_dir).map_err(io_err)?;
    let conflist = crate::network::guest_conflist(pod_network, 1280);
    let mut bytes = serde_json::to_vec_pretty(&conflist)
        .map_err(|error| Error::new(ErrorCode::Internal, error.to_string(), ""))?;
    bytes.push(b'\n');
    std::fs::write(config_dir.join("10-oci-cri-guest.conflist"), bytes).map_err(io_err)?;
    std::fs::write("/proc/sys/net/ipv4/ip_forward", "1").map_err(io_err)?;
    std::fs::write(
        format!("/proc/sys/net/ipv4/conf/{interface}/proxy_arp"),
        "1",
    )
    .map_err(io_err)?;

    config.cni_bin_dir = Some(PathBuf::from("/opt/cni/bin"));
    config.cni_conf_dir = Some(config_dir);
    eprintln!(
        "oci-cri: guest CNI subnet {}/{}, gateway {}; streaming on {address}",
        pod_network.subnet, pod_network.prefix, pod_network.gateway
    );
    Ok(())
}

/// Functionally verify that the running kernel can create and delete both a
/// `veth` pair and a `bridge` interface via `AF_NETLINK` (`NETLINK_ROUTE`).
#[cfg(target_os = "linux")]
fn probe_guest_cni() -> Result<(), String> {
    let fd = unsafe {
        libc::socket(
            libc::AF_NETLINK,
            libc::SOCK_RAW | libc::SOCK_CLOEXEC,
            libc::NETLINK_ROUTE,
        )
    };
    if fd < 0 {
        return Err(format!("rtnetlink socket: {}", io::Error::last_os_error()));
    }
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };

    let mut seq = 1_u32;
    for (kind, name, peer) in [
        ("veth", "rok-cni-veth0", Some("rok-cni-veth1")),
        ("bridge", "rok-cni-br0", None),
    ] {
        // Best-effort cleanup in case a previous probe left the interface behind.
        let _ = rtnetlink_exchange(&fd, &rtnetlink_dellink_message(seq, name));
        seq += 1;

        let create = rtnetlink_newlink_message(seq, name, kind, peer);
        seq += 1;
        if let Err(err) = rtnetlink_exchange(&fd, &create) {
            return Err(format!("{kind} create: {err}"));
        }

        let delete = rtnetlink_dellink_message(seq, name);
        seq += 1;
        if let Err(err) = rtnetlink_exchange(&fd, &delete) {
            return Err(format!("{kind} delete: {err}"));
        }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn rtnetlink_exchange(fd: &OwnedFd, request: &[u8]) -> io::Result<()> {
    let sent = unsafe {
        libc::send(
            fd.as_raw_fd(),
            request.as_ptr().cast::<libc::c_void>(),
            request.len(),
            0,
        )
    };
    if sent < 0 {
        return Err(io::Error::last_os_error());
    }
    let mut reply = [0_u8; 512];
    let received = unsafe {
        libc::recv(
            fd.as_raw_fd(),
            reply.as_mut_ptr().cast::<libc::c_void>(),
            reply.len(),
            0,
        )
    };
    if received < 0 {
        return Err(io::Error::last_os_error());
    }
    parse_rtnetlink_ack(&reply[..received as usize])
}

fn push_nla(buf: &mut Vec<u8>, attr_type: u16, payload: &[u8]) {
    let len = (4 + payload.len()) as u16;
    buf.extend_from_slice(&len.to_ne_bytes());
    buf.extend_from_slice(&attr_type.to_ne_bytes());
    buf.extend_from_slice(payload);
    let pad = (4 - (buf.len() % 4)) % 4;
    buf.resize(buf.len() + pad, 0);
}

fn cstr_bytes(value: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(value.len() + 1);
    out.extend_from_slice(value.as_bytes());
    out.push(0);
    out
}

fn build_nlmsg(msg_type: u16, flags: u16, seq: u32, attrs: &[u8]) -> Vec<u8> {
    let total_len = (NLMSG_HDRLEN + IFINFOMSG_LEN + attrs.len()) as u32;
    let mut msg = Vec::with_capacity(total_len as usize);
    msg.extend_from_slice(&total_len.to_ne_bytes());
    msg.extend_from_slice(&msg_type.to_ne_bytes());
    msg.extend_from_slice(&flags.to_ne_bytes());
    msg.extend_from_slice(&seq.to_ne_bytes());
    msg.extend_from_slice(&0_u32.to_ne_bytes());
    msg.resize(NLMSG_HDRLEN + IFINFOMSG_LEN, 0);
    msg.extend_from_slice(attrs);
    msg
}

fn rtnetlink_newlink_message(seq: u32, name: &str, kind: &str, peer: Option<&str>) -> Vec<u8> {
    let mut linkinfo = Vec::new();
    push_nla(&mut linkinfo, IFLA_INFO_KIND, &cstr_bytes(kind));
    if let Some(peer_name) = peer {
        let mut peer_info = vec![0_u8; IFINFOMSG_LEN];
        push_nla(&mut peer_info, IFLA_IFNAME, &cstr_bytes(peer_name));
        let mut info_data = Vec::new();
        push_nla(&mut info_data, VETH_INFO_PEER, &peer_info);
        push_nla(&mut linkinfo, IFLA_INFO_DATA, &info_data);
    }
    let mut attrs = Vec::new();
    push_nla(&mut attrs, IFLA_IFNAME, &cstr_bytes(name));
    push_nla(&mut attrs, IFLA_LINKINFO, &linkinfo);
    build_nlmsg(
        RTM_NEWLINK,
        NLM_F_REQUEST | NLM_F_ACK | NLM_F_EXCL | NLM_F_CREATE,
        seq,
        &attrs,
    )
}

fn rtnetlink_dellink_message(seq: u32, name: &str) -> Vec<u8> {
    let mut attrs = Vec::new();
    push_nla(&mut attrs, IFLA_IFNAME, &cstr_bytes(name));
    build_nlmsg(RTM_DELLINK, NLM_F_REQUEST | NLM_F_ACK, seq, &attrs)
}

fn parse_rtnetlink_ack(buf: &[u8]) -> io::Result<()> {
    if buf.len() < NLMSG_HDRLEN + 4 {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "truncated rtnetlink ack",
        ));
    }
    let msg_len = u32::from_ne_bytes(buf[0..4].try_into().unwrap()) as usize;
    if msg_len < NLMSG_HDRLEN + 4 || buf.len() < msg_len {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid rtnetlink message length",
        ));
    }
    let msg_type = u16::from_ne_bytes(buf[4..6].try_into().unwrap());
    if msg_type != NLMSG_ERROR {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unexpected rtnetlink message type {msg_type}"),
        ));
    }
    let error = i32::from_ne_bytes(buf[16..20].try_into().unwrap());
    if error == 0 {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(-error))
    }
}

#[cfg(target_os = "linux")]
fn guest_uplink() -> Option<(String, Ipv4Addr, u8)> {
    let mut head = std::ptr::null_mut();
    if unsafe { libc::getifaddrs(&mut head) } != 0 {
        return None;
    }

    let mut result = None;
    let mut current = head;
    while !current.is_null() {
        let interface = unsafe { &*current };
        if !interface.ifa_addr.is_null()
            && !interface.ifa_netmask.is_null()
            && unsafe { (*interface.ifa_addr).sa_family as i32 } == libc::AF_INET
        {
            let name = unsafe { CStr::from_ptr(interface.ifa_name) }.to_str().ok();
            if let Some(name) = name.filter(|name| {
                *name != "lo"
                    && Path::new("/sys/class/net")
                        .join(name)
                        .join("device")
                        .exists()
            }) {
                let address = unsafe { &*(interface.ifa_addr.cast::<libc::sockaddr_in>()) }
                    .sin_addr
                    .s_addr
                    .to_ne_bytes();
                let netmask = unsafe { &*(interface.ifa_netmask.cast::<libc::sockaddr_in>()) }
                    .sin_addr
                    .s_addr
                    .to_ne_bytes();
                let address = Ipv4Addr::from(address);
                if let Some(prefix) = ipv4_prefix(Ipv4Addr::from(netmask)) {
                    result = Some((name.to_owned(), address, prefix));
                    break;
                }
            }
        }
        current = interface.ifa_next;
    }
    unsafe { libc::freeifaddrs(head) };
    result
}

fn ipv4_prefix(netmask: Ipv4Addr) -> Option<u8> {
    let mask = u32::from_be_bytes(netmask.octets());
    let prefix = mask.leading_ones();
    let expected = if prefix == 0 {
        0
    } else {
        u32::MAX << (32 - prefix)
    };
    (mask == expected).then_some(prefix as u8)
}

#[cfg(target_os = "linux")]
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

#[cfg(target_os = "linux")]
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

#[cfg(target_os = "linux")]
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

#[cfg(target_os = "linux")]
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

#[cfg(target_os = "linux")]
fn io_err(err: io::Error) -> Error {
    Error::new(ErrorCode::Internal, err.to_string(), "")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ack_packet(error: i32) -> Vec<u8> {
        let len = (NLMSG_HDRLEN + 20) as u32;
        let mut buf = Vec::with_capacity(len as usize);
        buf.extend_from_slice(&len.to_ne_bytes());
        buf.extend_from_slice(&NLMSG_ERROR.to_ne_bytes());
        buf.extend_from_slice(&0_u16.to_ne_bytes());
        buf.extend_from_slice(&1_u32.to_ne_bytes());
        buf.extend_from_slice(&0_u32.to_ne_bytes());
        buf.extend_from_slice(&error.to_ne_bytes());
        buf.resize(len as usize, 0);
        buf
    }

    #[test]
    fn builds_aligned_veth_and_bridge_rtnetlink_requests() {
        let veth = rtnetlink_newlink_message(7, "rok-cni-veth0", "veth", Some("rok-cni-veth1"));
        assert_eq!(veth.len() % 4, 0);
        assert_eq!(
            u32::from_ne_bytes(veth[0..4].try_into().unwrap()) as usize,
            veth.len()
        );
        assert_eq!(
            u16::from_ne_bytes(veth[4..6].try_into().unwrap()),
            RTM_NEWLINK
        );
        assert_eq!(
            u16::from_ne_bytes(veth[6..8].try_into().unwrap()),
            NLM_F_REQUEST | NLM_F_ACK | NLM_F_EXCL | NLM_F_CREATE
        );
        assert_eq!(u32::from_ne_bytes(veth[8..12].try_into().unwrap()), 7);
        assert!(veth.windows(14).any(|w| w == b"rok-cni-veth0\0"));
        assert!(veth.windows(14).any(|w| w == b"rok-cni-veth1\0"));
        assert!(veth.windows(5).any(|w| w == b"veth\0"));

        let bridge = rtnetlink_newlink_message(8, "rok-cni-br0", "bridge", None);
        assert_eq!(bridge.len() % 4, 0);
        assert!(bridge.windows(12).any(|w| w == b"rok-cni-br0\0"));
        assert!(bridge.windows(7).any(|w| w == b"bridge\0"));
        assert!(bridge.len() < veth.len());

        let del = rtnetlink_dellink_message(9, "rok-cni-br0");
        assert_eq!(del.len() % 4, 0);
        assert_eq!(
            u16::from_ne_bytes(del[4..6].try_into().unwrap()),
            RTM_DELLINK
        );
        assert_eq!(
            u16::from_ne_bytes(del[6..8].try_into().unwrap()),
            NLM_F_REQUEST | NLM_F_ACK
        );
    }

    #[test]
    fn parses_rtnetlink_ack_success_and_errno() {
        parse_rtnetlink_ack(&ack_packet(0)).expect("zero error is an ACK");

        let err = parse_rtnetlink_ack(&ack_packet(-libc::EOPNOTSUPP)).unwrap_err();
        assert_eq!(err.raw_os_error(), Some(libc::EOPNOTSUPP));

        let truncated = parse_rtnetlink_ack(&[0_u8; 8]).unwrap_err();
        assert_eq!(truncated.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn validates_contiguous_ipv4_netmasks() {
        assert_eq!(ipv4_prefix(Ipv4Addr::new(255, 255, 255, 0)), Some(24));
        assert_eq!(ipv4_prefix(Ipv4Addr::new(255, 255, 0, 0)), Some(16));
        assert_eq!(ipv4_prefix(Ipv4Addr::new(0, 0, 0, 0)), Some(0));
        assert_eq!(ipv4_prefix(Ipv4Addr::new(255, 255, 255, 1)), None);
    }

    /// Run with `cargo test -p oci-cri serve::tests::live_probe_creates_and_deletes_veth_and_bridge -- --ignored`
    /// inside a Linux environment with `CAP_NET_ADMIN` and `CONFIG_VETH` + `CONFIG_BRIDGE`.
    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "requires Linux CAP_NET_ADMIN and kernel veth+bridge support"]
    fn live_probe_creates_and_deletes_veth_and_bridge() {
        probe_guest_cni().expect("veth+bridge rtnetlink probe should succeed");
        assert!(!Path::new("/sys/class/net/rok-cni-veth0").exists());
        assert!(!Path::new("/sys/class/net/rok-cni-veth1").exists());
        assert!(!Path::new("/sys/class/net/rok-cni-br0").exists());
    }
}
